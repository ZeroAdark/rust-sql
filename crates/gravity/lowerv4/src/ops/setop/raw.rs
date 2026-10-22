//! setop's raw half in lowerv4: the streamed sorted SetOp's compare of a key ordered by its type's built-in btree comparator,
//! entered through the contract's landing pad (kernels contract.rs `entry3`; stream.rs declares it). Owner setop.
#![allow(unsafe_code)]

/// AGA-30: the streamed sorted SetOp's key compared by its type's built-in btree comparator (numeric_cmp, bpcharcmp,
/// uuid_cmp, ...): the comparator of resolution id `pc & 0xffff_ffff` under collation `pc >> 32` over the datums `a` and
/// `x` (a row's value, or a held copy), as PostgreSQL's sort support calls it once per pair, through KeySem's bound-call
/// table (`keysem::call::order`): -1 / 0 / 1, or 2 when the bind or the call raised (its error stashed), as the IR reads it.
pub(crate) struct StreamHostedCmp;

impl gravity_kernels::contract::Host for StreamHostedCmp {
    type Args = (u64, u64, u64);

    unsafe fn run(_ctx: *mut gravity_abi::Ctx, (a, x, pc): (u64, u64, u64)) -> u64 {
        match gravity_fn::typesem::keysem::call::order(pc as u32, (pc >> 32) as u32, a, x) {
            Ok(o) => o as i8 as i64 as u64,
            Err(e) => {
                gravity_fn::stash_error(e);
                2
            }
        }
    }

    /// A panic in the comparator reads as raised.
    fn fail(_a: (u64, u64, u64)) -> u64 {
        2
    }
}

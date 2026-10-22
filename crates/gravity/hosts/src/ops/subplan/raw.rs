//! `ops::subplan`'s raw half in hosts (rules R6 / R7: the family's unsafe code lives here): the lifetime extension the ARRAY
//! sublink accumulator (stores.rs `ArrayAcc`) needs, a named function with its SAFETY text.
#![allow(unsafe_code)]
use mcx::{Mcx, MemoryContext};

/// The accumulator's build context `cx` at `'static` (stores.rs `ArrayAcc`: its `state` and `result` live in it).
pub(super) fn acc_mcx(cx: &MemoryContext) -> Mcx<'static> {
    // SAFETY: the context is boxed (a stable address) and outlives every value placed in it: `state` and `result` are
    // cleared before each reset and drop before `cx` (field order).
    unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(cx.mcx()) }
}

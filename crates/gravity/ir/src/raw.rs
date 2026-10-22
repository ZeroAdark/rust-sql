//! The IR's unsafe steps (rule R6, its raw home): an allocator handle into a program's root, and `Program`'s `Send` /
//! `Sync`.
#![allow(unsafe_code)]

use crate::{IrA, Program};
use mcx::SendRoot;

/// A new handle into `root` for a vector of the program that holds `root`.
pub(crate) fn handle(root: &mut SendRoot) -> IrA {
    // SAFETY: `SendRoot::handle`'s contract, as `Program` keeps it: the handle goes into a vector of the program whose root
    // this is; the program drops its vectors before its root (field order); a vector grows, shrinks or frees only under the
    // program's `&mut` (every IR type but `Program` is neither `Send` nor `Sync`, and nothing allocates through `&`).
    unsafe { root.handle() }
}

/// Another handle into the root `a` is a handle into (`Mint`, `Function::mint`: taken under the program's `&mut`).
pub(crate) fn again(a: &IrA) -> IrA {
    // SAFETY: as `handle`: the new handle is held beside the same root, by the same program.
    unsafe { a.again() }
}

// SAFETY: a program owns its root and every vector allocated in it (`handle`), and they move together: no IR value leaves
// its program, and every IR type but `Program` is `!Send`, so no part of one program is on another thread.
unsafe impl Send for Program {}
// SAFETY: through `&Program` nothing allocates or frees: `IrA` is not `Clone`, `Mint` needs `&mut`, and the root is
// private (its reads, `bytes`, touch no state a write could race).
unsafe impl Sync for Program {}

/// A new handle into `root` for a vector of the `JitTags` that holds `root`.
pub(crate) fn tags_handle(root: &mut SendRoot) -> IrA {
    // SAFETY: `SendRoot::handle`'s contract, as `JitTags` keeps it: the handle goes into a vector of the tags whose root this
    // is; the tags drop their vectors before their root (field order); a vector grows, shrinks or frees only under the tags'
    // `&mut` (their vectors are private, nothing allocates through `&`).
    unsafe { root.handle() }
}

// SAFETY: `JitTags` owns its root and every vector allocated in it (`tags_handle`), and they move together, whole.
unsafe impl Send for crate::JitTags {}
// SAFETY: through `&JitTags` nothing allocates or frees (every write takes `&mut`); its reads touch no state a write races.
unsafe impl Sync for crate::JitTags {}

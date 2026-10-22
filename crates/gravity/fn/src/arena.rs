//! LEDGER2 step 3: an operator's arena, accounting only, over today's regions. A Gravity operator allocates in its
//! `OpArena`, whose ledger owner (memledger `op_begin`, under the statement or portal the creating thread runs) holds every
//! chunk the arena draws: a chunk is asked of the program's spill host (`gravity_abi::program::arena_chunk`, the driver's
//! `work_take` road: W, then G, then past G a mapped temp file) and charged to that owner once, there. What the arena
//! carves from its chunks posts nothing.
//!
//! The kernel-facing types live here so kernels reach them through the crates they may depend on:
//! - `AVec<T: ForgetSafe>` is thin (`ptr, len, cap`) and stores no handle: every call that touches its buffer takes the
//!   arena. Its buffer has the arena's id in the 8 bytes below it, and its capacity word carries the id's low 16 bits, so
//!   a call with another arena (or this one after a reset) panics, in release builds too: never a dangling write.
//! - `AMap` is a hashbrown map over the arena; it holds a refcounted handle, and `reset` panics while one is live. Its
//!   tables carry the same header.
//! - `ABytes` is a Copy `{chunk, offset, len}` into the arena, read through it. Raw carves (`alloc`, `alloc_zeroed`,
//!   `bytes`) never grow, so they carry no header.
//!
//! Nothing here has a `Drop` that frees through a raw pointer: the arena gives its chunks back at its reset or drop (the
//! host's, else the statement's end), and an `AVec` / `ABytes` is forgotten (its elements are `ForgetSafe`). An arena lives
//! inside its ctx's run: it resets or drops before the Node that owns the ctx ends its statement.

mod raw;

pub use gravity_abi::program::{ARENA_CHUNK_FIRST, ARENA_CHUNK_MAX};
pub use memledger::Owner;
// wave 2 (RULINGS-1 R1, commit 4): the explicit-owner charge for memory an arena cannot hold, reachable from
// gravity_kernels (its boundary allows gravity_fn, not memledger)
pub use memledger::charge::Door;
pub use memledger::{Charge, ChargedVec, Release, VALUE_FLOOR};
pub use raw::{ABytes, AMap, AVec, ArenaAlloc, ArenaSeed, ForgetSafe, Kind, OpArena, Refused, Usage, CHUNKS, UNCHARGED};

//! The pull-form merge's text columns (M4-EXIT F5, V4R-305; a helper of runs/merge_pull.rs): a held arm row's varlena
//! (non-key) column keeps a copy of its value, since the arm's index source moves on (its page unpinned or reused) while
//! the other arms read, as PostgreSQL's MergeAppend slot holds its own tuple. Each such column of each arm owns one buffer
//! (two frame words: its pointer and capacity), reused by every row the arm reads and grown by a statement pin only when a
//! value is larger: memory is the largest value per arm and column, never per row. The copy is PostgreSQL's detoasted
//! payload behind a 4-byte header (pg_detoast_datum_packed then VARDATA_ANY, as keyed/emit_key.rs `key_copy`), 0 for NULL.
//! The window's out row points into the emitting arm's buffer, which that arm refills only at the next window (after the
//! row, and a stop's re-emit of it, are done): the cell lives `Until::Row`. Merge keys stay by-value (props `merge_pull`).
//! Registers nothing. Owner M3-MERGE.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntCond, Type, Value};

use crate::frame::env::Env;
use crate::frame::open::{Base, OpenWord, OpenWords};

/// A text column's buffer of one arm: frame words (byte offsets) of its pointer and its capacity.
#[derive(Clone, Copy)]
pub(crate) struct Buf {
    pub w_ptr: i32,
    pub w_cap: i32,
}

/// Both words start at 0 (no buffer) at every open.
pub(crate) fn open_words(w: &mut OpenWords, bufs: &[Buf]) {
    for x in bufs {
        w.declare(OpenWord::open("merge pull copy", Base::Words, x.w_ptr, Type::I64, 0));
        w.declare(OpenWord::open("merge pull copy cap", Base::Words, x.w_cap, Type::I64, 0));
    }
}

/// `datum`'s copy in buffer `x` (0 for a NULL `datum`, whose caller passes 0); a failed detoast or pin raises HOST.
pub(crate) fn copy(b: &mut FunctionBuilder<'_>, env: &mut Env, x: Buf, datum: Value) -> Value {
    let h = b.declare_host_once("pull_copy", &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), crate::ops::append::raw::pull_copy as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
    let Some(words) = env.words else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a merge copy without the words site" }) };
    let at = b.ptr_add(words, env.zero, 1, x.w_ptr);
    let w = b.call_host(h, &[env.ctx, at, datum], Some(Type::I64)).expect("returns the copy");
    let (failed, l_ok) = (b.icmp(IntCond::Slt, w, env.zero), b.create_block(&[]));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    w
}

/// `datum`'s fixed-width image of `len` bytes (a name, a uuid: a by-reference type of a fixed length) copied into buffer
/// `x`, the copy's address the held word (0 for a NULL `datum`); a failed pin raises HOST (ops/append/raw.rs
/// `PullCopyFixed`).
pub(crate) fn copy_fixed(b: &mut FunctionBuilder<'_>, env: &mut Env, x: Buf, datum: Value, len: u32) -> Value {
    let h = b.declare_host_once("pull_copy_fixed", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<crate::ops::append::raw::PullCopyFixed> as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
    let Some(words) = env.words else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a merge copy without the words site" }) };
    let at = b.ptr_add(words, env.zero, 1, x.w_ptr);
    let n = b.iconst(Type::I64, len as i64);
    let w = b.call_host(h, &[env.ctx, at, datum, n], Some(Type::I64)).expect("returns the copy");
    let (failed, l_ok) = (b.icmp(IntCond::Slt, w, env.zero), b.create_block(&[]));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    w
}

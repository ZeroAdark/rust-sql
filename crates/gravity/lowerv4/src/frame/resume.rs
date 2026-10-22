//! The resume word's position (a source's place in its rows across a stop): read at a call's open, written back after
//! the last row taken. The frame keeps it in the context header's resume word, or a nest inner's own position word
//! (`Env::pos`). Sources that resume by row (src.result, src.values, hashjoin's drain and spill sources) read and write
//! it here.
use gravity_abi::ctx::hdr;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use gravity_kernels::util::RESUME_OPEN;

use crate::frame::env::Env;

/// The resume word's position: 0 at a fresh call (the open marked `RESUME_OPEN`), else the row the stop wrote back. A
/// nest inner's source keeps it in the inner's own position word (`Env::pos`, M8 B1b), written `RESUME_OPEN` per re-open.
pub(crate) fn resume_pos(b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
    let w = match env.pos { Some(o) => env.load_word(b, o), None => b.load(Type::I64, env.ctx, hdr::RESUME as i32) };
    let open = b.iconst(Type::I64, RESUME_OPEN);
    b.int_bin(IntBinOp::Sub, w, open)
}

/// The position after the last row taken, as the resume word.
pub(crate) fn write_pos(b: &mut FunctionBuilder<'_>, env: &Env, cur: Value) {
    let open = b.iconst(Type::I64, RESUME_OPEN);
    let w = b.int_bin(IntBinOp::Add, cur, open);
    match env.pos {
        Some(o) => env.store_word(b, o, w),
        None => b.store(env.ctx, hdr::RESUME as i32, w),
    }
}

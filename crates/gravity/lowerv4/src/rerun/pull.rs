//! A pulled set's read (RT-2f; nest.cte's object: `ProtoKind::Cte`, exec v4/m8_cte.rs): a reader asks the producer behind
//! a handle word for more rows through `m8_cte_pull`. Here for a reader outside the ctescan family: lockrows INCSTREAM's
//! sort stream (ops/sort/inc_stream.rs). Owner lockrows.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;

use crate::frame::env::Env;

/// Pull the set behind handle word `h` (a param word) for `want` rows: the call's status. Below zero the producer raised;
/// the host stashed its error and the caller raises it inside its own read, where PostgreSQL raises it.
pub(crate) fn pull(b: &mut FunctionBuilder<'_>, env: &Env, h: u16, want: Value) -> Value {
    let flags = HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() };
    let f = b.declare_host_once("m8_cte_pull", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), m8::m8_cte_pull as *const () as usize, flags);
    let hv = b.load(Type::I64, env.params, h as i32 * 8);
    b.call_host(f, &[env.ctx, hv, want], Some(Type::I64)).expect("m8_cte_pull returns its status")
}

//! Pointer resolution at build (abi-v2 §2, §5): the per-site targets from each source's kind, the
//! per-program targets from the owner's role; a Binding changes only the per-Call cells.
use gravity_abi::program::{ProgramBlock, PushFn};

use crate::hostimpl;
use crate::node::Role;

mod raw;
pub(crate) use raw::sites;

/// The push target of a block with no sink attached (built, detached, a worker without a leg).
pub(crate) const UNBOUND_SINK: (PushFn, *mut u8) = (Some(hostimpl::sink::unbound), core::ptr::null_mut());

pub(crate) fn program_block(role: Role, partial: *mut gravity_abi::Partials, env: *mut u8, owner: *mut u8) -> ProgramBlock {
    let leader = matches!(role, Role::Leader);
    ProgramBlock {
        push: gravity_abi::word(UNBOUND_SINK.0),
        spill: gravity_abi::word(Some(crate::mem::spill_host::spill)),
        arena_grow: gravity_abi::word(Some(hostimpl::arena::grow)),
        write: gravity_abi::word(Some(if leader { hostimpl::dml::write } else { hostimpl::dml::trap_write })),
        lock: gravity_abi::word(Some(if leader { hostimpl::dml::lock } else { hostimpl::dml::trap_lock })),
        partial: gravity_abi::word(partial),
        env: gravity_abi::word(env),
        sink: gravity_abi::word(UNBOUND_SINK.1),
        rec_open: gravity_abi::word(Some(gravity_abi::breaker::rec_open)),
        owner: gravity_abi::word(owner),
        pin: gravity_abi::word(Some(hostimpl::arena::pin_host)),
        instr: gravity_abi::word(core::ptr::null_mut()),
    }
}

//! The arms of an Append as one set (cards/M2.md L6 road `append.arms`, H05 R9-L11; nothing to port: the ladder and v1 ran
//! arms serially). append.rs declares, at arm 0 when the road is on (`Parallel::Arms`), the `Arms` protocol over the
//! union's `n` arms from arm 0's program pipeline; the runtime (exec/src/v4/m2.rs `Arms`) checks it and runs the arms'
//! pipelines one after another, each claiming its own morsels. Arm concurrency (the runner's section scheduling) and the
//! per-arm fold answer (`AnswerForm`) are V4R-39's card; row order is free (a plain Append promises none) and an ordered Append's arms
//! run in arm order either way. Registers road `append.arms`.
use gravity_program::protos::{Proto, ProtoKind};

use crate::frame::px::Px;
use crate::ops::Table;

crate::strategies! {
    /// UNION ALL arms parallel, answered per arm.
    APPEND_ARMS = 63, "append.arms", Append;
}

pub(crate) const ROWS: Table = Table { strategies: &[(APPEND_ARMS, &())], ..crate::ops::NONE };

/// The union of `n` arms whose arm 0 is pipeline `k`; `slot` the breaker the arms fold into as one production (D393: the
/// driver opens it at the first arm and folds the rest on into it), `NO_SLOT` when the arms end in the sink.
pub(crate) fn declare(px: &mut Px<'_, '_>, k: usize, n: u16, slot: u8) {
    let mut p = Proto::new(ProtoKind::Arms, k as u16);
    p.arg = n as u64;
    p.slot = slot;
    px.declare_proto(p);
}

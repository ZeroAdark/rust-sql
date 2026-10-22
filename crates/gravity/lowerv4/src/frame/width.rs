//! Roads `width.work` (H15a, R2-L05 R4-L09) and `width.write` (H15b, R12-L17): a pipeline's width comes from the bind's
//! exact facts, never the planner's selectivity. Each pipeline over a claimable source declares the program's `Width`
//! protocol (frame/program.rs `width`). The runner (exec/src/v4/m1.rs) hands it to the driver's `pipeline_width`, which
//! picks the leader alone (no section, no pool wake) when the claims chose fewer than two workers or a heap has fewer
//! blocks than two startup claims, and otherwise one worker per claim up to the claimed width. A statement that writes or
//! locks rows runs on the leader (bit 63): the driver's `statement_width` enforces it, and the declaration keeps the road
//! visible per pipeline. A narrower width never changes the rows. Budget 160 lines.
use gravity_program::protos::{Proto, ProtoKind};

use crate::face::policy::{WidthDecl, WidthImpl, WidthMech};
use crate::frame::px::Px;

crate::strategies! {
    /// Leader-only / narrower width from exact bind facts.
    WIDTH_WORK = 30, "width.work", Scan;
    /// Writes and locks leader-only, no pool wake.
    WIDTH_WRITE = 31, "width.write", Write;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table {
    strategies: &[(WIDTH_WORK, &WidthImpl(&BY_WORK)), (WIDTH_WRITE, &WidthImpl(&BY_WORK))],
    ..crate::ops::NONE
};

/// `ProtoKind::Width`'s `arg` bit 63: the pipeline writes or locks rows (program/src/v4.rs; the driver's
/// `WIDTH_LEADER_ONLY`). The low bits would carry a rows estimate, which the driver does not read: the exact facts decide.
const LEADER_ONLY: u64 = 1 << 63;

struct ByWork;
static BY_WORK: ByWork = ByWork;

impl WidthMech for ByWork {
    fn declare(&self, px: &mut Px<'_, '_>, k: usize, d: WidthDecl) {
        let arg = if d.leader_only { LEADER_ONLY } else { 0 };
        px.declare_proto(Proto::new(ProtoKind::Width, k as u16).arg(arg));
    }
}

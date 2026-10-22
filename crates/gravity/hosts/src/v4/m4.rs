//! M4's host implementations (M4-plan.md §6, cards/M4.md L2): the v4 faces over the ladder's write runtime. The runtime
//! itself stays in `hosts/src/write*.rs`, `trigger*.rs`, `lock.rs` and `index_maint.rs`; M4's host deltas H-a..H-i edit
//! those in place (owner M4-HOSTS for every delta, main 09-25; the M4-UPDATE / LOCK / TRIG / ROUTE / RET lanes read them),
//! each with its reader's level. L1 / L2 stub: M4 derives no source of its own; the row ops reach the bound `WriteHost` /
//! `LockHost` through the driver (`hostimpl/dml.rs`). Created by M4 (owner M4-HOSTS).
use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::Recipe;

/// No recipe kind of M4's own: a write's source sites derive through the earlier milestones' rows.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

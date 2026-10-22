//! Lowerer v4's host implementations (docs/gravity/lowerv4/plan.md §4.3): one module per milestone (`m<k>.rs`) exporting its
//! `derive` and `SIZE`, reached from registry.rs's one v4 line. A recipe whose host derives itself (`Recipe::Rung`, a
//! `HostRecipe`) needs no derive here. APPEND-ONLY: a milestone adds its module's name on its own line right above the
//! sentinel and edits nothing else in this file.
use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::recipe::DeriveFn;
use crate::ops::Recipe;

pub mod m4;
pub mod m6;
pub mod m7;
/// Each milestone's derive and its sources' largest size, in landing order (v5 serial step, landing 5:
/// was `milestones!`).
const DERIVES: &[(DeriveFn, usize)] = &[
    (crate::ops::agg::hosts::derive, crate::ops::agg::hosts::SIZE),
    (crate::ops::scan::hosts::derive, crate::ops::scan::hosts::SIZE),
    (crate::ops::sort::hosts::derive, crate::ops::sort::hosts::SIZE),
    (m4::derive, m4::SIZE),
    (crate::ops::srf::store::derive, crate::ops::srf::store::SIZE),
    (m6::derive, m6::SIZE),
    (m7::derive, m7::SIZE),
    (crate::ops::subplan::stores::derive, crate::ops::subplan::stores::SIZE),
    (crate::ops::window::object::derive, crate::ops::window::object::SIZE),
    (crate::instr::derive, crate::instr::SIZE),
];
/// Recipe `i`'s source when a v4 milestone owns its kind (the first that does), else `None`.
pub fn derive(r: &Recipe, i: usize, desc: &SiteDesc, a: ArenaRef) -> Option<SourceBox> {
    DERIVES.iter().find_map(|d| (d.0)(r, i, desc, a))
}

/// The largest source any v4 milestone derives (the driver's size hint).
pub const SIZE: usize = {
    let (mut m, mut i) = (0, 0);
    while i < DERIVES.len() {
        if DERIVES[i].1 > m {
            m = DERIVES[i].1;
        }
        i += 1;
    }
    m
};

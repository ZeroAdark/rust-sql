//! M6's host implementations (M6-plan.md §6, cards/M6.md L2): what needs this crate's dependencies behind a handle word
//! the kernels entries (host_abi/v4/m6.rs, m6_agg.rs) call through: the aggregate bracket (`AggArena`, V4R-269, in
//! m6_agg.rs), the hosted compare / hash bound by fmgr, the surrogate code map and the per-worker intern table. M6
//! derives no source of its own. Created by M6-HOSTS.
use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::Recipe;

pub use crate::ops::agg::trans::hosted::AggArena;

/// No recipe kind of M6's own: M6 derives nothing.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

//! M7's host implementations (M7-plan.md §6, cards/M7.md L2 hosts): `m7_absorb`'s side behind the handle word the kernels
//! entry (host_abi/v4/m7.rs) calls through, handing the probe pipeline's seeded `m` to the drain end's merge (no copy when
//! the layouts agree: `m` is laid in the end's layout at make time). An L1 stub: M7 derives no source of its own (the drain
//! source is IR over the build region) until its readers land. Created by M7-DRAIN.
use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::Recipe;

/// No recipe kind of M7's own yet: the L1 stub derives nothing.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

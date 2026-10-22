//! M2's host implementations (M2-plan.md §6, cards/M2.md L3): the hosts M2's sources run on beyond the ones that already
//! derive (heap range, btree index and bitmap in registry.rs's `v3_r1` / `v3_r1b` rows). The heap page window's extras are
//! m2_heap.rs's. No M2 host keeps a heap condition cache (the user 09-24): no source here publishes a `granule_ident`.
//! Created by M2 (owner M2-HOSTS).
use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::Recipe;


/// No recipe kind of M2's own yet: every M2 site derives through registry.rs's earlier rows.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

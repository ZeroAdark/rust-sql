//! R1's host recipes (lowerv3 design §7.4): the heap range and pgrc granule sources (index and bitmap: `v3_r1b.rs`).
use gravity_driver::{box_source, ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::scan::pgrc::granule_source::GranuleRangeSource;
use crate::ops::scan::pgrc::merged::MergedRangeSource;
use crate::ops::scan::heap_source::HeapRangeSource;
use crate::ops::Recipe;

pub fn derive(r: &Recipe, i: usize, desc: &SiteDesc, a: ArenaRef) -> Option<SourceBox> {
    Some(match r.clone() {
        Recipe::Range(r) => {
            assert!(desc.is_range() && r.heap == desc.relation && r.natts == desc.natts(), "gravity: range recipe {i} does not match its site descriptor");
            // D3585: a relation-set site's source takes its execution's relations at the bind. D3586: every range source
            // knows its registry index, so the bind asks whether the initial pruning removed it
            match r.set.is_some() {
                true => box_source(HeapRangeSource::new(r).raw(desc.raw_pages).walk(desc.walk).at_set(i).at_site(i), a),
                false => box_source(HeapRangeSource::new(r).raw(desc.raw_pages).walk(desc.walk).at_site(i), a),
            }
        }
        Recipe::Granule(r) => {
            assert!(desc.is_range() && r.rel == desc.relation && r.natts == desc.natts(), "gravity: granule recipe {i} does not match its site descriptor");
            // sortmerge T2: a site the plan relies on for its key's order merges its parts' sorted streams
            match desc.is_merged() {
                true => box_source(MergedRangeSource::new(r, i, desc, a), a),
                false => box_source(GranuleRangeSource::new(r).at_site(i), a),
            }
        }
        _ => return None,
    })
}

pub const SIZE: usize = {
    let (c, d) = (core::mem::size_of::<HeapRangeSource>(), core::mem::size_of::<GranuleRangeSource>());
    if c > d { c } else { d }
};

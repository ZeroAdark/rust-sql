//! R1b's host recipes (lowerv3 design §7.4, §7.2.3): the index and bitmap sources, moved unchanged from `v3_r1.rs`
//! (main 09-23).
use gravity_driver::{box_source, ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::scan::bitmap_source::BitmapSource;
use crate::ops::scan::index_source::IndexSource;
use crate::ops::Recipe;

pub fn derive(r: &Recipe, i: usize, desc: &SiteDesc, a: ArenaRef) -> Option<SourceBox> {
    Some(match r.clone() {
        Recipe::Index(r) => {
            // the site's key cells are the recipe's key words (a row comparison key takes one per member), then a ranges
            // site's claim
            assert!(!desc.is_range() && r.heap == desc.relation && r.index == desc.index && r.words.len() == desc.key_words() && r.natts == desc.natts(), "gravity: recipe {i} does not match its site descriptor");
            // M2: a site that keeps a RowId lane takes N-row index-only windows (hosts v4/m2_index.rs)
            let rowid = desc.lanes().iter().any(|l| l.id == gravity_program::LaneId::RowId);
            // a leaves site's leader: its share of the run's leaf walk, else the serial walk (index_leaves.rs)
            if let Some(l) = r.leaves().filter(|_| desc.leaves() && crate::ops::scan::index_probe::lanes_served(desc)).cloned() {
                return Some(crate::ops::scan::index_leaves::derive_leader(IndexSource::new(r).batched(rowid), &l, i, a));
            }
            // a ranges site's leader: its claims' ranges, else the serial walk (index_ranges.rs)
            if let Some(g) = r.ranges().filter(|_| desc.ranges() && crate::ops::scan::index_probe::lanes_served(desc)).cloned() {
                return Some(crate::ops::scan::index_ranges::derive_leader(IndexSource::new(r).batched(rowid), &g, i, a));
            }
            box_source(IndexSource::new(r).batched(rowid), a)
        }
        Recipe::Bitmap(r, recheck) => {
            // the site's key cells are the recipe's words, as an index recipe's (a null test none, a row member one each)
            assert!(!desc.is_range() && r.heap == desc.relation && r.index == desc.index && r.words.len() == desc.nkeys() && r.natts == desc.natts(), "gravity: bitmap recipe {i} does not match its site descriptor");
            // M2: a site that keeps an Offset lane takes one window per heap page (hosts v4/m2_bitmap.rs); D2988: a RowId lane,
            // windows of whole pages that may span several; at registry entry `i`, whose recheck each bind takes from the
            // statement's `Resolved` (a parked Node's next execution)
            box_source(BitmapSource::new(r, recheck).paged_by(desc).at_entry(i), a)
        }
        _ => return None,
    })
}

pub const SIZE: usize = {
    let (a, b) = (core::mem::size_of::<IndexSource>(), core::mem::size_of::<BitmapSource>());
    let ab = if a > b { a } else { b };
    // D3014O: a worker's probe of an index site (`HostRegistry::derive_worker_source`)
    let c = crate::ops::scan::index_probe::SIZE;
    let abc = if ab > c { ab } else { c };
    // a leaves site's leader (the serial index source and its leaf walk) and worker
    let d = crate::ops::scan::index_leaves::SIZE;
    let abcd = if abc > d { abc } else { d };
    // a ranges site's leader (the serial index source and its ranges) and worker
    let e = crate::ops::scan::index_ranges::SIZE;
    if abcd > e { abcd } else { e }
};

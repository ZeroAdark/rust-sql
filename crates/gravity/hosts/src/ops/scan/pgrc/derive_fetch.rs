//! R7's host recipes (lowerv3 design §7.4): the by-rowid fetch site.
use gravity_driver::{box_source, ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::scan::pgrc::fetch_site::FetchSource;
use crate::ops::Recipe;

pub fn derive(r: &Recipe, i: usize, desc: &SiteDesc, a: ArenaRef) -> Option<SourceBox> {
    let Recipe::Fetch(r) = r.clone() else { return None };
    assert!(desc.is_range() && r.rel == desc.relation && r.natts == desc.natts(), "gravity: fetch recipe {i} does not match its site descriptor");
    Some(box_source(FetchSource::new(r), a))
}

pub const SIZE: usize = core::mem::size_of::<FetchSource>();

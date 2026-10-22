//! The hosted aggregate bracket's kept columns (a `#[path]` child of hosts v4/m6_agg.rs, D345): a column a GroupAggregate
//! outputs outside its aggregates that decompose cannot key (a key the planner dropped under a nondeterministic collation
//! or a functional dependency) is the group's first input row's value, PG's representative tuple, held in a state slot
//! as aggregate `naggs + i`. Created by M6-HOSTS.
mod raw;

pub(crate) use raw::{combine, image_len};

use crate::ops::agg::trans::hosted::AggArena;

impl AggArena {
    /// Kept column `aggno`'s index, if it is one (D345).
    pub(crate) fn kept_of(&self, aggno: u32) -> Option<usize> {
        aggno.checked_sub(self.naggs).map(|j| j as usize).filter(|&j| j < self.kept.len())
    }
}

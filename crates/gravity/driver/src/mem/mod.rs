//! `mem` (v5 serial step, landing 5: module lines written by modexit.py). The lowerer's memory adapter lives here too:
//! the leases a pipeline holds and the reset unit (moved from lowerv4 mem.rs; the driver makes every context).
pub mod arena;
pub mod batch;
pub mod feedback;
pub mod fence;
pub mod giveback;
pub mod budget;
pub mod grant;
pub mod grow;
/// Memory step 6a's broker policy is the memory policy's (`mempolicy::broker`); its driver path stays.
pub use mempolicy::broker as policy;
pub mod spill;
pub mod spill_host;
pub mod reap;
pub mod reclaim;
pub mod region;
pub mod resident;
pub mod root;
pub mod runcodec;

use gravity_program::{LifeId, Until};

/// A pipeline's leases (S6, `decompose::breakers::leased`): the life it produces and the lives it reads, each named by the
/// consumer (I3); the lowerer reads them from the graph, never keeps a held set of its own.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Leases {
    pub produces: Option<LifeId>,
    pub reads: Vec<LifeId>,
}

impl Leases {
    /// Whether this pipeline holds `life` open (it produces or reads it).
    pub fn holds(&self, life: LifeId) -> bool {
        self.produces == Some(life) || self.reads.contains(&life)
    }
}

/// The reset unit of a site context that no operator retains from: the window. The frame and every placement read it
/// here, so a change is this one line.
pub const fn reset_unit() -> Until {
    Until::Window
}

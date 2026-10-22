//! The policy face (plan §3.1; H15 H16): how wide a statement runs (width by work, leader-only writes), and the
//! per-statement floor that ENTRY builds (H16, cross-crate). The lowerer declares its inputs; the driver and the entry
//! decide at bind. Width never changes rows: a narrower run returns PG's result.
use crate::frame::px::Px;

crate::hook_variants! {
    /// Width by work (H15).
    "H15" pub enum Width {
        /// (a) exact bind facts (unanswered granules, heap blocks) pick leader-only or a narrower width, with no pool wake;
        /// partitions are sized to the result.
        Work = Strategy(WIDTH_WORK),
        /// (b) a statement with a write or a row lock runs leader-only, with no pool wake.
        Write = Strategy(WIDTH_WRITE),
        /// A walk's sections run on fewer workers.
        Walk = Strategy(WIDTH_WALK),
    }
}

crate::hook_variants! {
    /// The per-statement floor (H16 a–e). ENTRY builds it as one cross-crate road (`floor`).
    "H16" pub enum Floor {
        /// (a) a front-key hit before lowering, a lean bind, no per-execution copies.
        FrontKey = Strategy(FLOOR),
        /// (b) hosts bound for a prepared statement's life. Locks, cid, snapshot and statement triggers stay per execution.
        BoundHosts = Strategy(FLOOR),
        /// (c) a cold budget: the first execution goes to the interpreter tier when a compile would exceed the cell's noise.
        ColdBudget = Strategy(FLOOR),
        /// (d) an unconditional process-image warm.
        Warm = Strategy(FLOOR),
        /// (e) the front key also covers writes and SPI statements (RI checks).
        Writes = Strategy(FLOOR),
    }
}

/// What a program declares for width by work (H15). The driver reads it with the bind's exact facts:
/// - `work`: the width follows the unanswered granules or heap blocks;
/// - `leader_only`: a write or a row lock (H15 b);
/// - `walk`: walk sections may run narrower.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WidthDecl {
    pub work: bool,
    pub leader_only: bool,
    pub walk: bool,
}

/// The width a statement runs at: the leader alone (no pool wake), or up to `n` workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidthPick {
    Leader,
    Workers(u16),
}

/// Road width.work / width.write's mechanism (`WidthImpl`, H15): pipeline `k`'s width inputs declared to the driver (the
/// program's `Width` protocol), which picks the width from the bind's exact facts.
pub trait WidthMech: Sync {
    fn declare(&self, px: &mut Px<'_, '_>, k: usize, d: WidthDecl);
}

pub struct WidthImpl(pub &'static dyn WidthMech);

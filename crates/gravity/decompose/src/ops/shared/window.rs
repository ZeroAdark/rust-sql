//! The window frame descriptors other families read; their methods stay with ops/window.
use types_nodes::Node;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameMode {
    Rows,
    Range,
    Groups,
}

#[derive(Clone, Copy, Debug)]
pub enum FrameBound<'p> {
    UnboundedPreceding,
    Preceding(Node<'p>),
    CurrentRow,
    Following(Node<'p>),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exclude {
    None,
    CurrentRow,
    Group,
    Ties,
}

/// The walk's cursor positions a function reads (`FrameCursor::needs` bits): the current row's
/// position, the partition's bounds, the peer group's bounds, the frame's bounds.
pub const CURSOR_ROW: u8 = 1;
pub const CURSOR_PARTITION: u8 = 2;
pub const CURSOR_PEERS: u8 = 4;
pub const CURSOR_FRAME: u8 = 8;

/// The frame cursor the function emitter is handed per window function: the frame's mode and
/// bounds over the partition-sorted run, the exclusion, and which of the walk's positions the
/// function's body reads. The emitter open-codes the function over these positions (`FrameEmitter`).
#[derive(Clone, Copy, Debug)]
pub struct FrameCursor<'p> {
    pub mode: FrameMode,
    pub start: FrameBound<'p>,
    pub end: FrameBound<'p>,
    pub exclude: Exclude,
    pub needs: u8,
}

//! The admit facade (v5 admit landing 8a): a refusal is a coordinate, the node, the family it belongs to and the gap
//! that stops it. `refuse` unwinds with a `Refuse` payload; the one catch (lowerv4 `catch`) turns it into the
//! statement's refusal, so a refusal raised through here reads exactly as the `refuse!` it replaces.
pub use gravity_decompose::admit::Family;

/// Why a node is not served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gap {
    /// A plan shape the family does not lower yet (its static text is the seam text).
    Shape(&'static str),
    /// A known-broken shape, refused until its filed bug's verified root fix: `Gap::Bug(D(2194), WHY)` in the family's
    /// admit.rs, deleted with the fix and its witness. The D-id must be an open row of BUGS-OPEN.tsv (lint R10
    /// `--open-bugs`). The catch reads it exactly as `Shape(WHY)`: the same reason, the same seam text, never counted
    /// as a bug.
    Bug(D, &'static str),
    /// A scan of relation `relid` whose table AM v4's sources cannot read.
    Am(u32),
    /// A broken lowering invariant: counted as a bug by the catch.
    Invariant(&'static str),
}

/// A filed bug's D-id: `D(2194)` is D2194.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct D(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refuse {
    pub node: i32,
    pub family: Family,
    pub gap: Gap,
}

/// Refuse the statement at `r.node`: unwinds to the one catch.
pub fn refuse(r: Refuse) -> ! {
    std::panic::resume_unwind(Box::new(r)) // alloc: Cold
}

/// A lowering invariant does not hold at `node`: `refuse` with `Gap::Invariant`.
pub fn invariant(node: i32, family: Family, what: &'static str) -> ! {
    refuse(Refuse { node, family, gap: Gap::Invariant(what) })
}

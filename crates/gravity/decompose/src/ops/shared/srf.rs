//! The set-returning function descriptors other families read; their methods stay with ops/srf.
use gravity_program::ArenaVec;
use types_nodes::list::{NodeList, OidList};
use types_nodes::Node;

/// The set-returning function library's entries (by `pg_proc` OID); `Scalar` is a FROM-clause
/// function returning one row, `Other` any other set-returning function (its body the library's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SrfKind {
    SeriesInt { width: u8, step: bool },
    SeriesNumeric { step: bool },
    SeriesTime { tz: bool },
    Subscripts,
    Unnest,
    RegexpMatches,
    RegexpSplit,
    StringToTable,
    JsonbArrayElements { text: bool },
    JsonbEach { text: bool },
    JsonbObjectKeys,
    JsonArrayElements { text: bool },
    JsonEach { text: bool },
    JsonObjectKeys,
    JsonbPathQuery,
    Scalar,
    Other,
    /// A FROM-clause entry that is not a call (D3): an inlined SQL function's body, a constant-folded call — a `Const`,
    /// `RowExpr`, `CASE`, cast … evaluated once for one row (a composite value spreads over `cols`); `funcid` 0, no arguments.
    Expr,
}

/// One function of the loop: its target-list position (`resno`; a FROM-clause function's first
/// output column), the call, its kind, the columns it yields (`cols` > 1: a record).
#[derive(Clone, Copy)]
pub struct SrfFn<'p> {
    pub resno: i16,
    pub node: Node<'p>,
    pub funcid: u32,
    pub args: &'p NodeList<'p>,
    pub kind: SrfKind,
    pub cols: u16,
    /// The FROM clause's column definition list's types (a RECORD function); empty when none.
    pub coltypes: &'p OidList<'p>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SrfPlace {
    TargetList,
    From { scanrelid: u16, ordinality: bool },
}

/// The SRF loop the pipeline drives (ruling addendum): per input row every function is opened on
/// its arguments and the loop emits one output row per step while any function still yields,
/// exhausted ones padded with NULL; the plain target-list entries repeat per emitted row.
pub struct SrfDesc<'p> {
    pub place: SrfPlace,
    pub functions: ArenaVec<SrfFn<'p>>,
    pub tlist: &'p NodeList<'p>,
}

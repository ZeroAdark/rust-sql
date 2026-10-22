//! The aggregate classification vocabulary (decompose.md §2.6): the class of every built-in aggregate (`aggtab.rs`), the slot each class folds into (`Trans`: the frozen kinds and five named needs) and the call's modifiers as fields.
pub(crate) use gravity_abi::breaker::SlotKind;

/// One `pg_aggregate` row as the generator reads it (the catalog's columns the classification uses).
pub struct AggRow {
    pub fnoid: u32,
    pub name: &'static str,
    pub args: &'static [u32],
    pub kind: u8,
    pub ndirect: u8,
    pub transfn: &'static str,
    /// `aggtransfn`'s pg_proc oid: its properties (strictness) are the effects home's (`effects::builtin_func`).
    pub transfn_oid: u32,
    pub finalfn: &'static str,
    pub transtype: u32,
    pub combinefn: &'static str,
    pub initval: Option<&'static str>,
    pub sortop: u32,
    pub class: Class,
}

/// The extremum's type class: how `min` / `max` order the argument in its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ext {
    Int,
    /// `pg_lsn` / `xid8` / `oid`: unsigned, ordered through the sign-flipped image (an oid word past 2^31 orders above
    /// every smaller one, D2348).
    U64,
    /// `tid`: (block, offset) packed in order.
    Tid,
    F64,
    Num,
    Text,
    Bytes,
    Opaque,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stat {
    VarPop,
    VarSamp,
    StddevPop,
    StddevSamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regr {
    Sxx,
    Syy,
    Sxy,
    AvgX,
    AvgY,
    R2,
    Slope,
    Intercept,
    CovarPop,
    CovarSamp,
    Corr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bit {
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collect {
    Array,
    ArrayOfArrays,
    String,
    Json,
    JsonObject,
    Xml,
    Range,
    RangeIntersect,
    AnyValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ordered {
    PercentileDisc,
    PercentileCont,
    Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hypo {
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
}

/// The transition-slot class (the table's column): what the fold accumulates, by the catalog's transition and final functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    CountStar,
    CountAny,
    CountPair,
    SumI64,
    SumMoney,
    SumI128,
    SumNum,
    SumF32,
    SumF64,
    SumInterval,
    AvgI128,
    AvgNum,
    AvgF64,
    AvgInterval,
    Min(Ext),
    Max(Ext),
    BoolAnd,
    BoolOr,
    BitI64(Bit),
    BitVar(Bit),
    Stat3F64(Stat),
    Stat3I128(Stat),
    Stat3Num(Stat),
    Stat6F64(Regr),
    Collect(Collect),
    OrderedSet(Ordered),
    Hypothetical(Hypo),
    /// A user-defined aggregate (no catalog row; decision 79): the generic transition over a `Var` state.
    User,
}

/// The transition slot a class folds into: a frozen `gravity_abi` kind, or one of the five named needs (decompose.md §7 n).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trans {
    Abi(SlotKind),
    Bits,
    Interval16,
    Stat24,
    Stat48,
    Var,
}

impl Trans {
    pub const fn abi(self) -> Option<SlotKind> {
        match self {
            Trans::Abi(k) => Some(k),
            _ => None,
        }
    }

    pub const fn bytes(self) -> u8 {
        match self {
            Trans::Abi(k) => k.bytes(),
            Trans::Bits | Trans::Var => 8,
            Trans::Interval16 => 16,
            Trans::Stat24 => 24,
            Trans::Stat48 => 48,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    Simple,
    Initial,
    Final,
    Intermediate,
}

impl Split {
    /// `nodes.h`'s `AGGSPLITOP_COMBINE` (0x01) and `AGGSPLITOP_SKIPFINAL` (0x02); the serialize bits ride with them.
    pub const fn of(aggsplit: u32) -> Split {
        match (aggsplit & 1 != 0, aggsplit & 2 != 0) {
            (false, false) => Split::Simple,
            (false, true) => Split::Initial,
            (true, false) => Split::Final,
            (true, true) => Split::Intermediate,
        }
    }

    pub const fn combines(self) -> bool {
        matches!(self, Split::Final | Split::Intermediate)
    }

    pub const fn skips_final(self) -> bool {
        matches!(self, Split::Initial | Split::Intermediate)
    }
}

/// The modifiers of one aggregate call, as fields (decompose.md §2.6): the fold's slot and final are functions of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mods {
    pub distinct: u8,
    pub order: u8,
    pub filter: bool,
    pub direct: u8,
    pub split: Split,
    pub star: bool,
}

impl Class {
    /// Pass 1's two bounds of a two-pass top ordered first by this aggregate of a column (`desc`: its direction), as
    /// `(witness, best)` extremes of the column per key, `true` = `max` (gtopn GT-1). The witness is a value one group of
    /// the key reaches or betters (the group of the extreme's row reaches a same-direction extreme; every group's min,
    /// max or avg is at least the key's min under DESC, at most its max under ASC); the best is a value no group of the
    /// key betters (its max under DESC, its min under ASC). `best` is `None` when the witness is it (max DESC, min ASC);
    /// `None` for an aggregate the column's extremes do not bound.
    pub fn two_pass_bounds(self, desc: bool) -> Option<(bool, Option<bool>)> {
        match (self, desc) {
            (Class::Max(_), true) => Some((true, None)),
            (Class::Min(_), false) => Some((false, None)),
            (Class::Min(_) | Class::AvgI128, true) => Some((false, Some(true))),
            (Class::Max(_) | Class::AvgI128, false) => Some((true, Some(false))),
            _ => None,
        }
    }

    /// The fold's result depends on the input order (ORDER BY inside the call is honoured through a `Var` list).
    pub const fn order_sensitive(self) -> bool {
        matches!(self, Class::SumF32 | Class::SumF64 | Class::AvgF64 | Class::Stat3F64(_) | Class::Stat6F64(_) | Class::Collect(_) | Class::OrderedSet(_) | Class::Hypothetical(_) | Class::User)
    }

    /// The classes whose DISTINCT form is the pair set (ir-probe §14): the group's (keys, argument) identity set folded on the emit side.
    /// It reads the class and the DISTINCT modifier, never a spelling: normalize's `distinct_count` gives a count(*) over a
    /// subquery grouped by one NOT NULL column (GROUP BY or SELECT DISTINCT, hashed or sorted) as this count(DISTINCT k).
    pub const fn pair_fold(self) -> bool {
        matches!(self, Class::CountAny | Class::SumI64 | Class::SumI128 | Class::AvgI128 | Class::Min(Ext::Int) | Class::Max(Ext::Int))
    }
}

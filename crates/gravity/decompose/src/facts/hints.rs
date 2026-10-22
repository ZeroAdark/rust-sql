use types_nodes::plannodes::{Agg, GravityRewrite, Sort};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hint {
    DistinctHashed,
    ZoneTopK { attno: i16, desc: bool, nulls_first: bool },
    ZoneWalk { attno: i16, desc: bool, nulls_first: bool },
    TwoPass { attno: i16, floor_desc: bool, floor_nulls_first: bool },
}

impl Hint {
    pub const fn of_rewrite(r: GravityRewrite) -> Hint {
        match r {
            GravityRewrite::ZoneTopK { attno, desc, nulls_first } => Hint::ZoneTopK { attno, desc, nulls_first },
            GravityRewrite::ZoneWalk { attno, desc, nulls_first } => Hint::ZoneWalk { attno, desc, nulls_first },
            GravityRewrite::TwoPass { attno, floor_desc, floor_nulls_first } => Hint::TwoPass { attno, floor_desc, floor_nulls_first },
        }
    }

    pub fn of_agg(a: &Agg<'_>) -> [Option<Hint>; 2] {
        // `distinctHashed` is not declared (§3 decision 51: the pair set is the form whatever the planner's flag;
        // the admission's `hint` gate keeps the runner-bound rewrites out, not the distinct family)
        [None, a.gravity_rewrite.map(Hint::of_rewrite)]
    }

    pub fn of_sort(s: &Sort<'_>) -> Option<Hint> {
        s.gravity_rewrite.map(Hint::of_rewrite)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Hint::DistinctHashed => "distinctHashed",
            Hint::ZoneTopK { .. } => "ZoneTopK",
            Hint::ZoneWalk { .. } => "ZoneWalk",
            Hint::TwoPass { .. } => "TwoPass",
        }
    }

    pub const fn contract(self) -> &'static str {
        match self {
            Hint::DistinctHashed => "an identity-set breaker (Distinct) per (group, aggregate) under the aggregate's budget",
            Hint::ZoneTopK { .. } => "ordered granule claims on the key; the final-group rule closes a group when its key leaves the claim window",
            Hint::ZoneWalk { .. } => "ordered granule claims on the sort key; a bounded heap (TopK) of n rows",
            Hint::TwoPass { .. } => "pass 1: a count table on the key (HashAgg, exported keys + granule marks); pass 2: the admission probe after the scan's quals",
        }
    }

    pub const fn milestone(self) -> &'static str {
        match self {
            Hint::DistinctHashed | Hint::ZoneTopK { .. } | Hint::TwoPass { .. } => "D2",
            Hint::ZoneWalk { .. } => "D3",
        }
    }
}

//! Fold order: whether a group's aggregates give a result PostgreSQL accepts over its rows in any order, and whether
//! its min / max keep PostgreSQL's member on any walk (graph.rs re-exports both for the lowerings that ask). The
//! per-call rule they read is the facts home's (`facts::order::order_free`).
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm};

/// D853: whether every aggregate of a group gives a result PostgreSQL accepts over its rows in any order, so a DISTINCT
/// group may feed its riders in its own order (D859) and, with `walk_free`, a plain end may walk a page last to first
/// and fold a window in registers before its record. The facts home's one rule (`facts::order::order_free`) answers per
/// call: float, money and interval sums and averages and float variances are free under the raising licence
/// (`raises_by_order`, `license_raising`: an overflow raised ahead re-runs the statement in plan order; that run and a
/// statement that acts keep the order, `facts::order::plan_order`; D2597: the float folds raise wherever another order
/// could, and money / interval keep PG's walk order, `effects::visible::fold_visible`); bit strings' length mismatch
/// and every collecting, user or ordered call keep the order. A DISTINCT call the pair set ends (`PairCount` /
/// `PairFold`) is asked without its DISTINCT: the pair set dedups it and feeds the distinct values. The class list
/// below is kept for a split call and an ORDER BY the home does not decide (an exact integer fold is the same in any
/// order). Min / max of every type are free (D862: any member equal to PostgreSQL's passes): a fold that meets the rows
/// in another order may keep another member equal to PG's (numeric 1.0 / 1.00, float8 -0 / 0, interval '1 day' / '24
/// hours', bpchar's trailing blanks, a nondeterministic collation's equal strings, arrays of those), always an input
/// member's own image, which the rigs accept by their companion query (rig_aggtie's min / max equal member, v5probe's
/// comparator). A kept pinned column (D345: the group's first row, PG's representative tuple) keeps PG's row order.
pub fn order_free(g: &GroupDecl<'_>) -> bool {
    use gravity_decompose::facts::order;
    use gravity_decompose::ops::shared::agg::FinalForm;
    use gravity_decompose::ops::shared::agg_class::Class::*;
    use gravity_decompose::ops::shared::agg_class::{Ext, Mods};
    let exact = |c| matches!(c, CountStar | CountAny | CountPair | SumI64 | SumI128 | SumNum | AvgI128 | AvgNum | BoolAnd | BoolOr | BitI64(_))
        || matches!(c, Min(Ext::Int | Ext::U64 | Ext::Tid) | Max(Ext::Int | Ext::U64 | Ext::Tid));
    let home = |d: &gravity_decompose::ops::shared::agg::AggDecl<'_>| {
        let pair = matches!(d.final_, FinalForm::PairCount | FinalForm::PairFold);
        order::order_free(d.class, &if pair { Mods { distinct: 0, ..d.mods } } else { d.mods })
    };
    let kept = g.form == GroupForm::Plain && g.empty_no_row && !g.pinned.is_empty();
    !kept && g.aggs.iter().all(|d| exact(d.class) || home(d))
}

/// `order_free`, and every min / max shows the member PostgreSQL's forward scan meets first: the plain end's reverse page
/// walk and window carry (heap_pages.rs, plain.rs), perf transforms of a group that is served either way. They keep the
/// printed member because an expression over a min / max prints which member it got (octet_length(max(bpchar)),
/// m6_d862_text_minmax), which no rig can judge against PG's equal set; a whole min / max column is judged (rig_aggtie's
/// min / max equal member) and is free where order_free serves, beside a DISTINCT. Members that print alike: text /
/// bytea under a byte-equal collation (typesem one_image), an opaque type typesem calls one image (inet / cidr, enum,
/// timetz), numeric only in the one-dscale MinNum / MaxNum registers (D862's rule, kept for the walk). A hashed group's
/// keys likewise (heap_pages.rs walks pages last to first under its end too): a record keeps the image of one member of
/// each key, the one its walk meets first (M6 Decision 7 lets it be any, and an expression over the key prints it), so
/// every key groups by an image that is its value's bytes (no user `=`): equal members are one image and print alike.
pub fn walk_free(g: &GroupDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::agg::Register;
    use gravity_decompose::ops::shared::agg_class::Class::*;
    use gravity_decompose::ops::shared::agg_class::Ext;
    let member = |d: &gravity_decompose::ops::shared::agg::AggDecl<'_>| match d.class {
        Min(Ext::Text | Ext::Bytes) | Max(Ext::Text | Ext::Bytes) => d.coll.bytes_eq && args_one_image(d.fnoid),
        Min(Ext::Opaque) | Max(Ext::Opaque) if args_one_image(d.fnoid) => true,
        Min(Ext::Num | Ext::Opaque) | Max(Ext::Num | Ext::Opaque) => matches!(d.reg, Register::MinNum | Register::MaxNum),
        _ => true,
    };
    // the group's equality is its image's: the datum's bits, or a text / byte string's bytes (the key's image:
    // a nondeterministic collation's text is `Collated`, bpchar `Bpchar`)
    let key = |k: &gravity_decompose::ops::shared::agg::KeyDecl<'_>| {
        k.user_eq == 0 && (k.image.is_raw() || k.image.is_text() || k.image.is_bytes())
    };
    order_free(g) && g.aggs.iter().all(member) && g.keys.iter().all(key)
}

/// Whether every aggregate of group `g` gives the same bits over its rows in any order: counts, integer and numeric sums and
/// averages, the bool and integer bit folds, min / max (which member they keep is `walk_free`'s). heap_pages.rs reverses
/// the page walk under a hashed group's end only then: a float, money or interval fold is `order_free` under the raising
/// licence but rounds by its input order, and PostgreSQL's hashed Agg feeds each group its rows in the seq scan's order.
pub fn bits_free(g: &GroupDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::agg_class::Class::*;
    g.aggs.iter().all(|d| matches!(d.class, CountStar | CountAny | CountPair | SumI64 | SumI128 | SumNum | AvgI128 | AvgNum | BoolAnd | BoolOr | BitI64(_) | Min(_) | Max(_)))
}

/// An opaque min / max whose argument types typesem calls one image (inet / cidr, enum, timetz): a tie's member prints
/// the same whichever row the fold met first (`walk_free`'s member; agg's hosted.rs `sort_free` in a sorted keyed record).
pub fn one_image_extremum(d: &gravity_decompose::ops::shared::agg::AggDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::agg_class::{Class, Ext};
    matches!(d.class, Class::Min(Ext::Opaque) | Class::Max(Ext::Opaque)) && args_one_image(d.fnoid)
}

/// OID-23: whether every argument type of built-in aggregate `fnoid` has one image per equal value (typesem `one_image`:
/// text, bytea; not bpchar, whose trailing blanks compare equal). A user aggregate has no row and fails closed.
pub(crate) fn args_one_image(fnoid: u32) -> bool {
    gravity_fn::typesem::args_one_image(gravity_decompose::ops::agg::trans::table::row(fnoid).map(|r| r.args))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OID-23: min / max keep PG's member by typesem's one-image answer, not an aggregate oid list: text (2129 / 2145)
    /// and bytea (6395 / 6396) are one image, and so are the opaque inet (3564 / 3565), enum (3526 / 3527) and timetz
    /// (2124 / 2140); bpchar (2244 / 2245), interval (2128 / 2144), arrays (2050 / 2051) and records (6373 / 6374) are
    /// not, a user aggregate fails closed.
    #[test]
    fn min_max_member_by_typesem() {
        for f in [2129, 2145, 6395, 6396, 3564, 3565, 3526, 3527, 2124, 2140] {
            assert!(args_one_image(f), "{f}");
        }
        for f in [2244, 2245, 2128, 2144, 2050, 2051, 6373, 6374, 16_384] {
            assert!(!args_one_image(f), "{f}");
        }
    }
}

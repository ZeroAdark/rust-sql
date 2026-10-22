//! Road `srf.limitcut` (H32, R8-L08; L3, registers with its L4 reader; cards/M5.md `srf/limit_cut.rs`, M5-plan §3.8,
//! Decisions 3 and 21): the LIMIT stop reaches the set loop. The stop itself is the frame's: step/limit.rs checks its
//! count after each row it passes and leaves the pipeline once the count is reached (`full`), before the set loop steps
//! again, so the next element is never computed and the next input row never fetched; OFFSET rows are produced (the
//! Limit skips them after the set made them), WITH TIES takes one more row, and `LIMIT 0` ends the window before any row
//! reaches the loop (PG's Limit returns before pulling its child). What this road decides is where that cut may meet
//! the set loop without changing PG's result, error or side effects (`cut`):
//! - in the target list always: PG's ProjectSet is lazy too (a Materialize-mode function still computes its whole set
//!   for its input row, inside its own call);
//! - in FROM, where PG materializes the whole set before the first row, only within D21's scope (the user 09-26): a
//!   built-in, row-at-a-time (ValuePerCall), non-volatile set function with no side effects, whose every error is raised
//!   at open (the iterators, the library entries flagged so; the caller says whether its form streams). The errors PG's
//!   full materialization alone would hit are resource limits (53xxx, 54xxx, 57014), outside error-vs-value; every
//!   other error still matches PG. Anything else (a PL or user function, D6; a Materialize-mode function; a library entry
//!   that can raise mid-set) runs to its end first, as PG does, and the cut applies to the read only.
//! With the road off (`gravity_road_off=srf.limitcut`) every FROM set drains first; the target list is unchanged.
//! Owner M5-PSET.
use gravity_decompose::effects;
use gravity_decompose::ops::shared::srf::{SrfFn, SrfKind, SrfPlace};

use crate::face::step::ExpandKind;
use crate::graph::Gx;
use crate::ops::Table;

crate::strategies! {
    /// LIMIT stop reaches the set loop.
    SRF_LIMITCUT = 103, "srf.limitcut", SrfLimit;
}

pub(crate) const ROWS: Table = Table { strategies: &[(SRF_LIMITCUT, &LimitCut)], ..crate::ops::NONE };

/// The road's value; its rule is `cut`.
pub(crate) struct LimitCut;

/// Where a LIMIT's stop meets one set function's loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    /// A target-list set: PG's ProjectSet stops as lazily, so the cut is exact.
    Lazy,
    /// A FROM set within D21's scope: streamed, the cut ends it early.
    Stream,
    /// A FROM set PG materializes whose run to the end can be told apart: drained first, the cut applies to the read.
    Drain,
}

/// Function `f`'s cut at `place`. `streams`: its form raises every error at open and yields row by row (srf/iter.rs's
/// iterators, a library entry flagged so), which the FROM caller knows from the form it chose.
pub(crate) fn cut(gx: &Gx<'_, '_>, f: &SrfFn<'_>, place: SrfPlace, streams: bool) -> Cut {
    rule(place, f.kind, effects::of_call(gx.facts, f.funcid).acts, streams, ExpandKind::LimitCut.on())
}

/// The cut as a table: the target list always; FROM only within D21 (a library entry, whose call does not act, a
/// streaming form, the road on). `acts`: effects' answer for the call (a VOLATILE or unknown function).
fn rule(place: SrfPlace, kind: SrfKind, acts: bool, streams: bool, on: bool) -> Cut {
    if place == SrfPlace::TargetList {
        return Cut::Lazy;
    }
    // a library entry (decompose's kind_of names built-in functions only) whose call does not act has no side effects
    let library = !matches!(kind, SrfKind::Other | SrfKind::Scalar | SrfKind::Expr);
    match on && streams && library && !acts {
        true => {
            fires::fire("srf.limitcut"); // shapecheck fire signal (REGISTRY NEW-lowerv4-a2-36)
            Cut::Stream
        }
        false => Cut::Drain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TL: SrfPlace = SrfPlace::TargetList;
    const FROM: SrfPlace = SrfPlace::From { scanrelid: 1, ordinality: false };
    const SERIES: SrfKind = SrfKind::SeriesInt { width: 4, step: false };

    /// A target-list set is cut lazily whatever it is (PG's ProjectSet stops too), road on or off.
    #[test]
    fn a_target_list_set_is_cut_lazily() {
        for (kind, acts, streams, on) in [(SERIES, false, true, true), (SrfKind::Other, true, false, false), (SrfKind::JsonbArrayElements { text: false }, false, false, true)] {
            assert_eq!(rule(TL, kind, acts, streams, on), Cut::Lazy);
        }
    }

    /// D21's scope in FROM: a built-in streaming form that is not volatile streams; a user function (D6), a
    /// Materialize-mode or other C function (`Other`), a volatile one, a form that can raise mid-set, or the road off
    /// drains first (G4: the cut leaves those sets alone).
    #[test]
    fn from_streams_only_within_d21() {
        assert_eq!(rule(FROM, SERIES, false, true, true), Cut::Stream);
        assert_eq!(rule(FROM, SrfKind::JsonbArrayElements { text: false }, false, true, true), Cut::Stream);
        for (kind, acts, streams, on) in [
            (SrfKind::Other, true, true, true),
            (SrfKind::Other, false, true, true),
            (SERIES, true, true, true),
            (SERIES, false, false, true),
            (SERIES, false, true, false),
            (SrfKind::Expr, false, true, true),
            (SrfKind::Scalar, false, true, true),
        ] {
            assert_eq!(rule(FROM, kind, acts, streams, on), Cut::Drain, "{kind:?} acts {acts} streams {streams} on {on}");
        }
    }
}

//! A pipeline's end, read through the interface home (lint RB): the breaker a pipeline description ends in, to read or
//! to edit in place, or another description's end to take over whole (`with_end_of`). A family asks these instead of matching the description's end itself; `end_breaker` (pipeline `k`'s
//! breaker kind) and `ends_in_sink` beside them in iface.rs answer the two questions asked of a graph pipeline by number.
use gravity_abi::breaker::Breaker;
use gravity_program::spill::{self, Shape};
use gravity_program::{End, Pipeline};

/// The breaker pipeline description `d` ends in (kind, slot, size, budget, spill), `None` when it ends in the client's
/// sink. For a graph pipeline `k`: `breaker_of(&gx.pipelines()[k].desc)`; its slot alone is also `d.produces()`.
pub fn breaker_of(d: &Pipeline) -> Option<Breaker> {
    match d.ends_in {
        End::Breaker(b) => Some(b),
        End::Sink => None,
    }
}

/// `breaker_of`'s breaker to edit in place (a maker settling the kind, size or slot of its own description's breaker),
/// `None` for a sink end.
pub fn breaker_of_mut(d: &mut Pipeline) -> Option<&mut Breaker> {
    match &mut d.ends_in {
        End::Breaker(b) => Some(b),
        End::Sink => None,
    }
}

/// C10: description `d` with its end's answer past the grant settled as `shape` (`spill::plan`), when the lowered end
/// does otherwise than its declaration (a pass that never spills, a set PostgreSQL also keeps in memory); a sink end is
/// unchanged. The one way a lowering states that answer.
pub fn end_holds(d: Pipeline, shape: Shape) -> Pipeline {
    match d.ends_in {
        End::Breaker(b) => Pipeline { holds: Some(spill::plan(&b, shape)), ..d },
        End::Sink => d,
    }
}

/// Description `d` with the end of description `src`: its end (`ends_in`) and the fields that describe it (the end's
/// owner, the two-pass admit, the words, the top-k, finish, the stored row's bytes, width and rows, and a hash build's
/// rows: the rows its Hash node receives, D3355); every other field is `d`'s. A pipeline that takes over another's end
/// (a fused drain's probe ends as its drain ends) asks this instead of copying the fields itself.
pub fn with_end_of(src: &Pipeline, d: Pipeline) -> Pipeline {
    Pipeline { ends_in: src.ends_in, owner: src.owner, admit: src.admit, words: src.words, topk: src.topk, finish: src.finish, store_row_bytes: src.store_row_bytes, row_width: src.row_width, end_rows: src.end_rows, build_rows: src.build_rows, holds: src.holds, ..d }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_program::{End, Pipeline, Reason};

    /// A sink end has no breaker; a breaker end is read whole, and an edit through `breaker_of_mut` is the description's end.
    #[test]
    fn a_breaker_end_is_read_and_edited_whole() {
        let b = Breaker::sized(BreakerKind::Materialize, 64).in_slot(2);
        assert_eq!(super::breaker_of(&Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)), None);
        let mut p = Pipeline::serial(-1, End::Breaker(b), Reason::NoDrivingSite);
        assert_eq!(super::breaker_of(&p), Some(b));
        assert_eq!(super::breaker_of(&p).map(|b| b.slot), p.produces(), "the slot is the one the description produces");
        if let Some(e) = super::breaker_of_mut(&mut p) {
            *e = e.in_slot(5);
        }
        assert_eq!(p.ends_in, End::Breaker(b.in_slot(5)));
        assert!(super::breaker_of_mut(&mut Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)).is_none());
    }

    /// `with_end_of` carries the end and the fields that describe it from its source and keeps every other field.
    #[test]
    fn with_end_of_takes_the_end_and_keeps_the_rest() {
        let b = Breaker::sized(BreakerKind::Materialize, 64).in_slot(3);
        let src = Pipeline { finish: true, store_row_bytes: 40, row_width: 24, build_rows: 9, ..Pipeline::serial(-1, End::Breaker(b), Reason::NoDrivingSite) };
        let d = Pipeline { predicted_rows: 77, ..Pipeline::serial(4, End::Sink, Reason::NoDrivingSite) };
        let n = super::with_end_of(&src, d);
        assert_eq!(n.ends_in, End::Breaker(b));
        assert_eq!((n.finish, n.store_row_bytes, n.row_width, n.build_rows), (true, 40, 24, 9));
        assert_eq!((n.driving_site, n.predicted_rows), (d.driving_site, 77), "the other fields are the description's own");
    }
}

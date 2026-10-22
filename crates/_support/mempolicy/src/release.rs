//! When a running statement gives back what its scans are done with. A release buys room only past the cache's
//! budget (where a trim runs), or when memory is short; under it with room, it evicts nothing and costs the next run the
//! cursors and memos it reopens.

/// D3994 (level gate): `short` (`cache::memory_short`, read only at the cache's own level): with the box short of memory a
/// part every claim left goes back though the cache is inside its budget. The parts a statement deferred counted in the
/// cache's floor, which kept the broker's target at or above the cache's bytes, so under the cache's level alone no
/// release could come due (D3994's broker-on stress: 511 parts, 7.8 GiB held to the statement's end at B's free 0).
#[inline]
pub fn part_release_due(cache_bytes: u64, level: u64, short: bool) -> bool {
    cache_bytes > level || short
}

/// A uses-only release past the cache's level also gives back the dictionaries it opened, each with its part, unless
/// its breaker keeps images of them past the window that read them (`keeps_images`: a TopK's rendered rows, a hash
/// build's code cells); code words are rendered after the release by opening the dictionary again.
#[inline]
pub fn dicts_go_with_part(keeps_images: bool) -> bool {
    !keeps_images
}

#[cfg(test)]
mod tests {
    use super::{dicts_go_with_part, part_release_due};
    use crate::cache::{memory_short, ParkedSignals};

    #[test]
    fn a_breaker_that_keeps_code_words_gives_its_dictionaries_back_with_the_part() {
        assert!(dicts_go_with_part(false), "an aggregate, a Sort, a DISTINCT: code words, rendered again after the release");
        assert!(!dicts_go_with_part(true), "a TopK's rendered rows or a hash build's code cells point into the images");
    }

    #[test]
    fn a_part_goes_back_only_past_the_level() {
        assert!(!part_release_due(462 << 20, 2706 << 20, false), "a cache under its budget keeps the part");
        assert!(!part_release_due(2706 << 20, 2706 << 20, false), "at the target nothing is due");
        assert!(part_release_due(8 << 30, 1 << 30, false), "past the budget the part goes back");
        assert!(part_release_due(1, 0, false), "level 0: always (tests)");
        assert!(!part_release_due(u64::MAX, u64::MAX, false), "an unbounded cache: never");
    }

    /// D3994 (level gate): memory short, a part every claim left goes back inside the cache's budget (D3994's broker-on
    /// stress: B 15 GiB, free 0, the cache inside its budget, 511 parts held); a box with room keeps it (D3268).
    #[test]
    fn d3994_a_part_goes_back_inside_the_budget_when_memory_is_short() {
        const G: u64 = 1 << 30;
        let s = |free_b: u64| ParkedSignals { bar: 0, projected: 0, free_b, b: 15 * G, broker: true, bytes: 7 * G, budget: 8 * G };
        assert!(part_release_due(7 * G, 8 * G, memory_short(&s(0))), "B full: due inside the budget");
        assert!(!part_release_due(7 * G, 8 * G, memory_short(&s(12 * G))), "a box with room: held");
        assert!(!part_release_due(7 * G, 8 * G, memory_short(&s(u64::MAX))), "no snapshot yet: held");
    }
}

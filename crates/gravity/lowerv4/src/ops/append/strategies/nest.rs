//! D4569 (road `append.nest`): a plain SELECT's quiet, parameter-free nested loop over a plan Material above a UNION ALL
//! is copied into each arm (decompose join.rs puts the nest in the graph's `union_nests`, arm_nests.rs `per_arm` copies
//! it), so the union streams its arms through the nest instead of collecting them into a Materialize first: each copy
//! joins its own arm's rows against the one spool the inner's fill wrote (rerun/spool/readers.rs `copies`, `union_nest`),
//! the fill runs eagerly in pipeline order ahead of every copy (lazy.rs `fill_wait`), and D4532 widens a copy whose end
//! is order-free (ops/nestloop/widen.rs `spool`). The graph decides it, so exec admission reads the road before it
//! decomposes (`on`); the road-off text keys the program tiers and the front key (hosts residency.rs `tier_salt`, D311),
//! so the two arms of an A/B never share a program. On by default (not in road_off.rs `DEFAULT_OFF`); off under
//! `SET pgrust.gravity_road_off = 'append.nest'`, the union keeps its Materialize form.
use crate::ops::Table;

crate::strategies! {
    /// A SELECT's quiet union nest copied into the union's arms.
    APPEND_NEST = 141, "append.nest", Append;
}

pub(crate) const ROWS: Table = Table { strategies: &[(APPEND_NEST, &())], ..crate::ops::NONE };

/// Whether road append.nest is on for the statement exec admission is about to decompose: the option is read first (the
/// lowering reads it again at its top, road_off.rs `apply`; an unknown id is the statement's ERROR either way), so a `SET`
/// takes effect at the next statement.
pub fn on() -> bool {
    crate::road_off::read();
    crate::face::end::Parallel::ArmNest.on()
}

//! What a Memoize's cache shows the user when it purges (nodeMemoize.c cache_purge_all / remove_cache_entry): the
//! memoize family's sibling file of effects/visible.rs.
use gravity_decompose::effects::Volatility;

/// D_PURGE (ops/memoize/lower.rs `make_parts`): the most volatile call below a Memoize (its nest's facts) when a re-run
/// may purge the cache where a parameter outside its keys changed: a VOLATILE call there runs again at the purge's misses,
/// so the count of calls is PostgreSQL's only where the purge points are exactly PG's (`purge_exact`).
pub(crate) fn purge_calls(volatility: Volatility) -> bool {
    volatility == Volatility::Volatile
}

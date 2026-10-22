//! The effects home's lower layer in exec, one of its two (gravity_fn's fn/src/effects.rs is the other; lint HOME
//! effects lists decompose's home and these two, no more). The home reads PostgreSQL's own volatility reading of a plan
//! expression through it: `contain_volatile_functions` comes from the clauses crate, which exec links and decompose does
//! not.
use types_nodes::node_tree::Node;

/// Whether `n` calls a VOLATILE function by PostgreSQL's contain_volatile_functions (optimizer/util/clauses.c), failing
/// closed (true) where that reading errors.
pub(crate) fn contains_volatile(n: Node<'_>) -> bool {
    clauses::contain_volatile_functions(n).unwrap_or(true)
}

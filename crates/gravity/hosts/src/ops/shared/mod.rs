//! The helpers several host families share: a relation parked between executions and re-pinned, opened, a row's scan
//! position as a heap TID and back, a fetched heap row's cells, a computed index key's evaluation, and an index walk's
//! row bounds from the plan, and (D3267) the mapped road a host's bytes past work_mem take. A home, not a family: it imports
//! no family.
pub mod heap_fetch;
pub mod index_bound;
pub mod key_fn;
pub(crate) mod mapped;
pub(crate) mod raw;
pub mod rel;

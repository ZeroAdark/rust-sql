//! The lifetime home: when PostgreSQL 18.6's executor keeps a node's state across a rescan
//! and when it drops and rebuilds it, as one table keyed by the PG plan node and its executor flags (never by the lowered
//! breaker kind), plus the flag propagation that decides a Sort's or a Material's rewind. A node with no rule refuses by
//! name: nothing is kept by default. A site may keep what PG re-runs only where the fill's effects show no difference
//! (`resolve`). The runtime half, each word's declared reset scope, is lowerv4's `lifetime`. A site that
//! still decides keep, rebuild or reset outside the two halves is a row of lint/LIFETIME-DEBT (rule RL); the row goes when
//! its owner moves the decision here.
//! When PG evaluates an executor-parameter word or bound (the bound-at phase, C4) is `bound`'s answer.
mod bound;
mod keep;
mod rerun;
mod stores;

pub use bound::{sample_at, BoundAt, Feed};
pub use keep::{at_any_rescan, child_eflags, cte_scans, eflags_at, eflags_in, function_rescan, keep_rule, keeps_in, node_of, reach, resolve, shields, shields_in, Keep, Kept, Reach, Rescan, Site, Usual, BACKWARD, MARK, REWIND};
pub use rerun::{KeptBy, KeptRec};
pub(crate) use rerun::{changed_params, claim_refills, keep_at, keepable, kept_hashed, reexecuted, refills_left, rereads_run, rerun_set, sub_kept_fills, term_unwhole};
pub use stores::{root_store, subplan_stores, Store};

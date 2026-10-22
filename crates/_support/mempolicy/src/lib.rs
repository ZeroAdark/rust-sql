//! The memory policy: every decision about how much memory a consumer gets, who waits and when memory goes back, as a
//! plain function of a ledger snapshot and one table of constants. The ledger (`memledger`) only counts; mechanisms
//! ask here for every number and never compute their own. The crate depends on nothing in Gravity or the backend.

pub mod admit;
pub mod alloc;
pub mod broker;
pub mod cache;
pub mod consts;
pub mod curve;
pub mod decide;
pub mod grow;
pub mod hostpress;
pub mod keep;
mod knobs;
pub mod ladder;
pub mod load;
pub mod maps;
pub mod reclaim;
pub mod reconcile;
pub mod relief;
pub mod release;
pub mod sizes;
pub mod snapshot;
pub mod trace;
pub mod track;
pub mod transfer;
pub mod watchdog;

//! The table module's raw half (rule R6: its unsafe code lives here): the generic owner phase, whose region work is
//! raw-pointer work over the owner's layout. table/mod.rs re-exports it as `table::owner`, the path its readers name.
pub mod fold_code;
pub mod owner;

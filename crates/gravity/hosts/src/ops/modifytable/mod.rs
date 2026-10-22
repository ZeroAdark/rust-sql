//! `ops::modifytable` (v5 serial step, landing 5: module lines written by modexit.py).
pub mod index_maint;
/// W6-D1112: the family's raw home (R6): write_check.rs's left sides evaluated in the row's context.
pub(crate) mod raw;
pub mod trigger;
pub mod trigger_before;
pub mod trigger_stmt;
pub mod write;
pub mod write_check;
pub mod write_constraints;
pub mod write_generated;
pub mod write_index_am;
pub(crate) mod write_move;
pub mod write_partition;
pub mod write_route;
/// D774: RETURNING's system columns of the written row, a sub-file of this one (no hosts lib.rs line).
pub(crate) mod write_sys;
pub mod write_view;
/// H-d (Decision 8): the view UPDATE / DELETE host, a sub-file of this one (no hosts lib.rs line).
pub(crate) mod write_view_rows;

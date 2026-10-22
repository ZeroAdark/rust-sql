//! The write family's host entry: the C-ABI function the lowered IR calls for a row op (nodeModifyTable.c's
//! ExecInsert / ExecUpdate / ExecDelete on one row). `write_row` reaches `ProgramBlock.write` (`Program.write`, the ladder's
//! `h_write`), which the driver resolves to the bound write host (driver `hostimpl/dml/raw.rs` `write`, hosts
//! ops/modifytable). The lowerers (lowerv4 ops/modifytable rowop.rs `host_op`, onconflict.rs's conflict-site op) take its
//! address here. The entry lives in raw.rs (rules R6 / R8).
pub use super::raw::write_row;

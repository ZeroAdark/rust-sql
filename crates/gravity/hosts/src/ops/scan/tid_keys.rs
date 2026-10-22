//! The TID sources' key-cell images (hosts v4/m2_tid_sample.rs `TidSource`, split out for its budget): a `tid[]` array's
//! elements (`= ANY`), and D422's `CURRENT OF` cursor word (a refcursor's text or the execution's `CurrentOfExpr`).
//! Created by M2 (owner M2-SOURCES).
mod raw;
pub(crate) use raw::{cursor_cell, tid_array_cell, tid_cell};

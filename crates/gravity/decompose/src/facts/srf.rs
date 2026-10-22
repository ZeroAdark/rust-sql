//! Plan facts: the row order of a FROM-clause function scan WITH ORDINALITY. PostgreSQL's FunctionScan numbers the rows
//! it returns 1, 2, .. in the ordinality column (nodeFunctionscan.c `ordinal`), so its rows come in that column's
//! ascending order, never NULL, and its planner gives the scan that pathkey (build_function_pathkeys's ordinality
//! clause): a WindowAgg `OVER (ORDER BY ord)` reads it with no Sort. ops/srf.rs's `SrfPlace::From` loop emits the rows in
//! the same order, one pipeline, serial.
use crate::graph::{Col, Op, Order, OrderKey};
use crate::ops::shared::srf::SrfPlace;

/// The order of a pipeline whose first op is `op`: a FROM function scan WITH ORDINALITY's ordinality column ascending
/// (the column after every function's columns, numbered in the scan's own range-table entry), else none.
pub fn ordinality_order(op: Option<&Op<'_>>) -> Order {
    let mut o = Order::NONE;
    if let Some(Op::Srf(s)) = op {
        if let SrfPlace::From { scanrelid, ordinality: true } = s.place {
            let cols: u32 = s.functions.iter().map(|f| u32::from(f.cols)).sum();
            if let Ok(attno) = i16::try_from(cols + 1) {
                o.push(OrderKey { col: Col { varno: i32::from(scanrelid), attno }, desc: false, nulls_first: false });
            }
        }
    }
    o
}

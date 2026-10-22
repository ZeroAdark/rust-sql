//! Plan node families the server does not run: a Foreign Scan and a Custom Scan, and with them every read or write of a
//! foreign table through a plan (the row executor that ran them is removed). Such a statement raises 0A000 at its
//! admission, before the graph admission: it is never lowered, and its error names the foreign table. The plan
//! reader names what it holds (`gravity_decompose::removed::find`): a foreign partition the planner pruned is in no plan
//! node, and the statement is served. A routed write into a tree holding a foreign leaf raises the same error from the
//! leaf's refusal (route_leaves `FOREIGN_LEAF`, `leaf_refusal`). What runs no plan is unchanged: the foreign data DDL,
//! IMPORT FOREIGN SCHEMA, ANALYZE and TRUNCATE of a foreign table, COPY FROM into one.
use gravity_decompose::removed::Removed;
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED};

use super::PlannedStmt;

/// The error of a statement that reads or writes a foreign table.
const FOREIGN: &str = "foreign tables are not supported in this release";

/// The error of a statement whose plan holds a Custom Scan.
const CUSTOM: &str = "custom scans are not supported in this release";

/// The hint of `FOREIGN`: what still works, and what to do instead.
const FOREIGN_HINT: &str = "Foreign data wrappers (postgres_fdw, file_fdw) can be created and configured, but no statement can read or write a foreign table. Copy the data into a local table instead.";

/// The hint of `CUSTOM`.
const CUSTOM_HINT: &str = "A Custom Scan comes from an extension's planner hook; disable the extension that adds it for this statement.";

/// The statement's 0A000 when its plan holds a Foreign Scan or a Custom Scan or writes a foreign table, then its result
/// relations' table AMs' own refusal, then the refusal of a table AM that samples no rows under a TABLESAMPLE.
pub fn check<'p>(pstmt: &'p PlannedStmt<'p>) -> PgResult<()> {
    match gravity_decompose::removed::find(pstmt) {
        None => {
            crate::ops::modifytable::statement::admit_ams(pstmt)?;
            if crate::admit::is_explain_only() {
                return Ok(());
            }
            gravity_hosts::ops::scan::tid_sample::admit_samples(pstmt)
        }
        Some(r) => Err(error(r)),
    }
}

/// A routed write's refusal `why` that names a foreign leaf (`FOREIGN_LEAF`), as the statement's 0A000. `None` for any
/// other refusal, and for an EXPLAIN without ANALYZE, which prints the plan from the plan alone (the leaf is no result
/// relation of the plan, so it has no provider lines).
pub fn leaf_refusal(why: &str) -> Option<Box<PgError>> {
    if crate::admit::is_explain_only() || !why.contains(gravity_decompose::ops::modifytable::route_leaves::FOREIGN_LEAF) {
        return None;
    }
    Some(raise(FOREIGN, Some("A partition the statement routes its rows through is a foreign table.".into()), FOREIGN_HINT)) // alloc: Cold
}

/// `r` as PostgreSQL words a feature it does not support: the message, and a detail naming the table.
fn error(r: Removed) -> Box<PgError> {
    let detail = match r {
        Removed::ForeignScan { relid, writes } => {
            let verb = if writes { "writes" } else { "reads" };
            (relid != 0).then(|| format!("The plan {verb} foreign table {} through a Foreign Scan.", name(relid))) // alloc: Cold
        }
        Removed::ForeignTarget { relid } => Some(format!("The statement writes foreign table {}.", name(relid))), // alloc: Cold
        Removed::CustomScan => return raise(CUSTOM, Some("The plan holds a Custom Scan node.".into()), CUSTOM_HINT), // alloc: Cold
    };
    raise(FOREIGN, detail, FOREIGN_HINT)
}

/// The relation's name, quoted as PostgreSQL's messages quote it, or its OID when the catalog has no row.
fn name(relid: u32) -> String {
    let ctx = gravity_driver::mem::root::Root::new("GravityRemovedName"); // alloc: Cold
    let quoted = lsyscache::relation::get_rel_name(ctx.mcx(), relid as Oid).ok().flatten().map(|n| format!("\"{n}\"")); // alloc: Cold
    quoted.unwrap_or_else(|| format!("with OID {relid}")) // alloc: Cold
}

fn raise(msg: &str, detail: Option<String>, hint: &str) -> Box<PgError> {
    let e = PgError::error(msg).with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED).with_hint(hint); // alloc: Cold
    Box::new(match detail { // alloc: Cold
        Some(d) => e.with_detail(d),
        None => e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every removed family is 0A000; a Custom Scan has its own message.
    #[test]
    fn removed_families_raise_0a000() {
        for r in [Removed::ForeignScan { relid: 0, writes: false }, Removed::CustomScan] {
            assert_eq!(error(r).sqlstate(), ERRCODE_FEATURE_NOT_SUPPORTED, "{r:?}");
        }
        assert_eq!(error(Removed::ForeignScan { relid: 0, writes: true }).message(), FOREIGN);
        assert_eq!(error(Removed::CustomScan).message(), CUSTOM);
        assert_eq!(error(Removed::ForeignScan { relid: 0, writes: false }).hint(), Some(FOREIGN_HINT));
        assert_eq!(error(Removed::CustomScan).hint(), Some(CUSTOM_HINT));
    }
}

//! The entry seam's knobs (exec.md §4.7): what the seam offers to Gravity, and the per-backend report / facts switches.

use types_nodes::plannodes::PlannedStmt;
use types_nodes::CmdType;

use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob { name: "PGRUST_GRAVITY_REPORT", kind: Kind::Env, default: "unset", doc: "1: the served / fallthrough census lines per execution (read once per backend)" },
    Knob { name: "PGRUST_GRAVITY_FACTS", kind: Kind::Env, default: "unset", doc: "1: one `gravity facts:` LOG line per relation the plan names (scripts/gravity-facts-check.sh)" },
    Knob { name: "PGRUST_GRAVITY_POINT_PLANCHECK", kind: Kind::Env, default: "unset", doc: "1 (proof runs): pgrust.point_path's hand-built plan of a SELECT without FROM, and pgrust.point_noplan's reused plan, is compared with the planner's; a difference is one `gravity: point plancheck mismatch` LOG line (the planner's folding runs twice)" },
    Knob { name: "PGRUST_GRAVITY_MEMO_SLOTS", kind: Kind::Env, default: "unset", doc: "D1789 e2e only: <n> plan-memo slots per level (1..=MAX_PLANS, read once per backend); unset: MAX_PLANS" },
];

/// Whether the entry seam offers the statement to Gravity at all: one plannable query — a plan tree under CMD_SELECT/INSERT/UPDATE/DELETE/MERGE, PostgreSQL's `CMD_UTILITY` distinction (`ChoosePortalStrategy`). A utility statement (SET, SHOW, EXPLAIN, DECLARE, FETCH, COPY, DDL, transaction control …) runs on PostgreSQL's utility path, and the plans it runs (EXPLAIN ANALYZE's, a cursor's, COPY's) reach the entry seam at their own ExecutorStart. A rule-rewritten list (PORTAL_MULTI_QUERY) is offered query by query: the protocol runner leaves the list to the portal, whose ProcessQuery starts each query through the entry seam (e2e m1_write: a DO ALSO DELETE and its rule's SELECT are served).
pub fn offered(pstmt: &PlannedStmt<'_>) -> bool {
    matches!(pstmt.commandType, CmdType::CMD_SELECT | CmdType::CMD_INSERT | CmdType::CMD_UPDATE | CmdType::CMD_DELETE | CmdType::CMD_MERGE) && pstmt.planTree.is_some()
}

/// The fallthrough line of a statement the protocol runner does not offer: the utility path runs it.
pub const NOT_OFFERED: &str = "not offered: a utility statement runs on the utility path";

pub fn report_on() -> bool {
    super::env_on("PGRUST_GRAVITY_REPORT")
}

pub use guc_tables::consts::{POINT_PATH_OFF, POINT_PATH_ON, POINT_PATH_STRICT};

/// pgrust.point_path, the session's value (runner::point): `POINT_PATH_OFF`, `_ON` or `_STRICT`.
pub fn point_path() -> i32 {
    guc_tables::backing::pgrust_point_path()
}

pub use guc_tables::consts::{POINT_NOPLAN_OFF, POINT_NOPLAN_ON, POINT_NOPLAN_STRICT};

/// pgrust.point_noplan, the session's value (runner::point::skip): `POINT_NOPLAN_OFF`, `_ON` or `_STRICT`.
pub fn point_noplan() -> i32 {
    guc_tables::backing::pgrust_point_noplan()
}

/// The session's settings mark (`guc::store::store_mutation_count`: any SET, RESET, SET LOCAL, function SET clause,
/// rollback of one or reload moves it; a statement that sets nothing leaves it): runner::point::skip's memo slot holds
/// while it stays where it was at filing.
pub fn settings_at() -> u64 {
    guc::store::store_mutation_count()
}

/// A planner setting under which a point statement's plan depends on its constants in a way the memo of
/// runner::point::skip does not follow: constraint_exclusion on (the refutation proof reads them) or debug_parallel_query
/// not off (a Gather over every plan).
pub fn noplan_setting() -> Option<&'static str> {
    if guc_tables::vars::constraint_exclusion.read() == guc_tables::consts::CONSTRAINT_EXCLUSION_ON {
        return Some("constraint_exclusion");
    }
    if guc_tables::vars::debug_parallel_query.read() != guc_tables::consts::DEBUG_PARALLEL_OFF {
        return Some("debug_parallel_query");
    }
    None
}

/// pgrust.point_write, the session's value (runner::point S3: a point DELETE through the write and index hosts).
pub fn point_write() -> bool {
    guc_tables::backing::pgrust_point_write()
}

/// pgrust.point_keep, the session's value (runner::point S3 option (a): a point statement's pieces kept in S5's memo entry).
pub fn point_keep() -> bool {
    guc_tables::backing::pgrust_point_keep()
}

/// pgrust.point_portal_keep, the session's value (runner::point pkeep.rs: an S1x portal's bound probe kept across the
/// portals of one generic plan).
pub fn point_portal_keep() -> bool {
    guc_tables::backing::pgrust_point_portal_keep()
}

/// wal_level logical (XLogLogicalInfoActive: above replica, the one level past it): runner::point's writes decline.
pub fn logical_wal() -> bool {
    guc_tables::vars::wal_level.read() > guc_tables::consts::WAL_LEVEL_REPLICA
}

/// A planner tap (planner_seams::planning_tapped: pg_stat_statements' planner hooks), log_planner_stats or debug_print_plan
/// watches planning, so a statement planned without the planner (runner::point) declines.
pub fn planning_observed() -> bool {
    let tapped = planner_seams::planning_tapped::is_installed() && planner_seams::planning_tapped::call();
    tapped || guc_tables::backing::log_planner_stats() || guc_tables::backing::Debug_print_plan()
}

/// log_executor_stats: PortalRun prints a statement's EXECUTOR STATISTICS, so a run with no portal (runner::point) declines.
pub fn log_executor_stats() -> bool {
    guc_tables::backing::log_executor_stats()
}

/// D1789 (e2e only): `PGRUST_GRAVITY_MEMO_SLOTS=<n>` sizes a backend's plan-memo slots down from `MAX_PLANS` (1..=MAX_PLANS),
/// so a test fills every slot with a few checked-out plans; unset or out of range, `MAX_PLANS`. Read once per backend.
pub fn memo_slots() -> usize {
    super::env_u64("PGRUST_GRAVITY_MEMO_SLOTS").and_then(|n| usize::try_from(n).ok()).filter(|n| (1..=crate::MAX_PLANS).contains(n)).unwrap_or(crate::MAX_PLANS)
}

pub fn facts_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| super::env_on("PGRUST_GRAVITY_FACTS"))
}

//! misc.c + version.c slices.


use types_error::PgResult;
use types_tuple::NameData;

// Shaped like C's PG_VERSION_STR ("... on <triple>, ..."): collate.linux.utf8
// and infinite_recurse gate on version() ~ platform regexes ('linux-gnu'), so
// the target triple must appear in BOTH renderings. Which identity leads is
// GUC-controlled (pgrust.version_string_style, ruling 2026-08-06): the
// default postgres_first matches C PostgreSQL's own "PostgreSQL <ver> on ..."
// shape, so clients that parse the FIRST version number out of version()
// (duckdb-postgres's ExtractPostgresVersion took the legacy leading "0.3"
// for a pre-8.3 PostgreSQL and silently degraded) read the compatibility
// version, with pgrust's identity parenthesized; pgrust_first restores the
// legacy pgrust-led ordering. Both version numbers and the build id come from
// guc_tables::consts — the single source of truth, never re-hardcoded here.
// The `server_version` GUC stays exactly the plain compat number: that is
// what drivers parse for feature detection.
static VERSION_STR_POSTGRES_FIRST: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| version_str_postgres_first(&guc_tables::consts::pgrust_identity()));
static VERSION_STR_PGRUST_FIRST: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| version_str_pgrust_first(&guc_tables::consts::pgrust_identity()));

pub(crate) fn version_str_postgres_first(identity: &str) -> String {
    format!(
        "PostgreSQL {} ({identity}) on {}, 64-bit",
        guc_tables::consts::PG_COMPAT_VERSION,
        env!("PGRUST_TARGET_TRIPLE"),
    )
}

pub(crate) fn version_str_pgrust_first(identity: &str) -> String {
    format!(
        "{identity} (PostgreSQL {} compatible) on {}, 64-bit",
        guc_tables::consts::PG_COMPAT_VERSION,
        env!("PGRUST_TARGET_TRIPLE"),
    )
}

/// version()'s banner under the session's pgrust.version_string_style. Reads
/// the registered GUC's backing cell directly (the runtime_pool precedent:
/// always installed, never panics in unit tests).
pub fn pg_version_str() -> &'static str {
    if guc_tables::backing::pgrust_version_string_style()
        == guc_tables::consts::VERSION_STRING_PGRUST_FIRST
    {
        &VERSION_STR_PGRUST_FIRST
    } else {
        &VERSION_STR_POSTGRES_FIRST
    }
}

pub fn current_database() -> PgResult<NameData> {
    let dbname = dbcommands::get_database_name(init_small::globals::MyDatabaseId())?
        .expect("current database has a pg_database row");
    let mut db = NameData::default();
    db.namestrcpy(&dbname);
    Ok(db)
}

//! The builtin set-returning functions the lowering plans by their identity: each one's pg_proc OID (pg_proc.dat) and the
//! `SrfKind` it lowers as (ops/srf.rs, ops/shared/srf.rs). A user function, or a builtin not listed, is `Other` (it returns
//! a set) or `Scalar`.
use crate::ops::shared::srf::SrfKind;

// fmgroids.h's names (PostgreSQL 18.6 pg_proc.dat) of the builtin set-returning functions `kind_of` implements: a
// dispatch on the function's identity, so each OID is its own name, not a gate on a premise
pub const F_GENERATE_SERIES_INT4_INT4_INT4: u32 = 1066;
pub const F_GENERATE_SERIES_INT4_INT4: u32 = 1067;
pub const F_GENERATE_SERIES_INT8_INT8_INT8: u32 = 1068;
pub const F_GENERATE_SERIES_INT8_INT8: u32 = 1069;
pub const F_GENERATE_SERIES_NUMERIC_NUMERIC_NUMERIC: u32 = 3259;
pub const F_GENERATE_SERIES_NUMERIC_NUMERIC: u32 = 3260;
pub const F_GENERATE_SERIES_TIMESTAMP_TIMESTAMP_INTERVAL: u32 = 938;
pub const F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL: u32 = 939;
pub const F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL_TEXT: u32 = 6274;
pub const F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4_BOOL: u32 = 1191;
pub const F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4: u32 = 1192;
pub const F_UNNEST_ANYARRAY: u32 = 2331;
pub const F_REGEXP_MATCHES_TEXT_TEXT: u32 = 2763;
pub const F_REGEXP_MATCHES_TEXT_TEXT_TEXT: u32 = 2764;
pub const F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT: u32 = 2765;
pub const F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT_TEXT: u32 = 2766;
pub const F_STRING_TO_TABLE_TEXT_TEXT: u32 = 6160;
pub const F_STRING_TO_TABLE_TEXT_TEXT_TEXT: u32 = 6161;
pub const F_JSONB_ARRAY_ELEMENTS: u32 = 3219;
pub const F_JSONB_ARRAY_ELEMENTS_TEXT: u32 = 3465;
pub const F_JSONB_EACH: u32 = 3208;
pub const F_JSONB_EACH_TEXT: u32 = 3932;
pub const F_JSONB_OBJECT_KEYS: u32 = 3931;
pub const F_JSON_ARRAY_ELEMENTS: u32 = 3955;
pub const F_JSON_ARRAY_ELEMENTS_TEXT: u32 = 3969;
pub const F_JSON_EACH: u32 = 3958;
pub const F_JSON_EACH_TEXT: u32 = 3959;
pub const F_JSON_OBJECT_KEYS: u32 = 3957;
pub const F_JSONB_PATH_QUERY: u32 = 4006;
pub const F_JSONB_PATH_QUERY_TZ: u32 = 1179;

/// Every name above with its OID, in `kind_of`'s order (tests/srf_dispatch_names.rs checks each against pg_proc.dat).
pub const NAMES: &[(&str, u32)] = &[
    ("F_GENERATE_SERIES_INT4_INT4_INT4", F_GENERATE_SERIES_INT4_INT4_INT4),
    ("F_GENERATE_SERIES_INT4_INT4", F_GENERATE_SERIES_INT4_INT4),
    ("F_GENERATE_SERIES_INT8_INT8_INT8", F_GENERATE_SERIES_INT8_INT8_INT8),
    ("F_GENERATE_SERIES_INT8_INT8", F_GENERATE_SERIES_INT8_INT8),
    ("F_GENERATE_SERIES_NUMERIC_NUMERIC_NUMERIC", F_GENERATE_SERIES_NUMERIC_NUMERIC_NUMERIC),
    ("F_GENERATE_SERIES_NUMERIC_NUMERIC", F_GENERATE_SERIES_NUMERIC_NUMERIC),
    ("F_GENERATE_SERIES_TIMESTAMP_TIMESTAMP_INTERVAL", F_GENERATE_SERIES_TIMESTAMP_TIMESTAMP_INTERVAL),
    ("F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL", F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL),
    ("F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL_TEXT", F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL_TEXT),
    ("F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4_BOOL", F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4_BOOL),
    ("F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4", F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4),
    ("F_UNNEST_ANYARRAY", F_UNNEST_ANYARRAY),
    ("F_REGEXP_MATCHES_TEXT_TEXT", F_REGEXP_MATCHES_TEXT_TEXT),
    ("F_REGEXP_MATCHES_TEXT_TEXT_TEXT", F_REGEXP_MATCHES_TEXT_TEXT_TEXT),
    ("F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT", F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT),
    ("F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT_TEXT", F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT_TEXT),
    ("F_STRING_TO_TABLE_TEXT_TEXT", F_STRING_TO_TABLE_TEXT_TEXT),
    ("F_STRING_TO_TABLE_TEXT_TEXT_TEXT", F_STRING_TO_TABLE_TEXT_TEXT_TEXT),
    ("F_JSONB_ARRAY_ELEMENTS", F_JSONB_ARRAY_ELEMENTS),
    ("F_JSONB_ARRAY_ELEMENTS_TEXT", F_JSONB_ARRAY_ELEMENTS_TEXT),
    ("F_JSONB_EACH", F_JSONB_EACH),
    ("F_JSONB_EACH_TEXT", F_JSONB_EACH_TEXT),
    ("F_JSONB_OBJECT_KEYS", F_JSONB_OBJECT_KEYS),
    ("F_JSON_ARRAY_ELEMENTS", F_JSON_ARRAY_ELEMENTS),
    ("F_JSON_ARRAY_ELEMENTS_TEXT", F_JSON_ARRAY_ELEMENTS_TEXT),
    ("F_JSON_EACH", F_JSON_EACH),
    ("F_JSON_EACH_TEXT", F_JSON_EACH_TEXT),
    ("F_JSON_OBJECT_KEYS", F_JSON_OBJECT_KEYS),
    ("F_JSONB_PATH_QUERY", F_JSONB_PATH_QUERY),
    ("F_JSONB_PATH_QUERY_TZ", F_JSONB_PATH_QUERY_TZ),
];

/// The `SrfKind` each listed builtin lowers as, by its pg_proc OID under fmgroids.h's name (`NAMES`).
pub const fn kind_of(funcid: u32, retset: bool) -> SrfKind {
    match funcid {
        F_GENERATE_SERIES_INT4_INT4_INT4 => SrfKind::SeriesInt { width: 4, step: true },
        F_GENERATE_SERIES_INT4_INT4 => SrfKind::SeriesInt { width: 4, step: false },
        F_GENERATE_SERIES_INT8_INT8_INT8 => SrfKind::SeriesInt { width: 8, step: true },
        F_GENERATE_SERIES_INT8_INT8 => SrfKind::SeriesInt { width: 8, step: false },
        F_GENERATE_SERIES_NUMERIC_NUMERIC_NUMERIC => SrfKind::SeriesNumeric { step: true },
        F_GENERATE_SERIES_NUMERIC_NUMERIC => SrfKind::SeriesNumeric { step: false },
        F_GENERATE_SERIES_TIMESTAMP_TIMESTAMP_INTERVAL => SrfKind::SeriesTime { tz: false },
        F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL | F_GENERATE_SERIES_TIMESTAMPTZ_TIMESTAMPTZ_INTERVAL_TEXT => SrfKind::SeriesTime { tz: true },
        F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4_BOOL | F_GENERATE_SUBSCRIPTS_ANYARRAY_INT4 => SrfKind::Subscripts,
        F_UNNEST_ANYARRAY => SrfKind::Unnest,
        F_REGEXP_MATCHES_TEXT_TEXT | F_REGEXP_MATCHES_TEXT_TEXT_TEXT => SrfKind::RegexpMatches,
        F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT | F_REGEXP_SPLIT_TO_TABLE_TEXT_TEXT_TEXT => SrfKind::RegexpSplit,
        F_STRING_TO_TABLE_TEXT_TEXT | F_STRING_TO_TABLE_TEXT_TEXT_TEXT => SrfKind::StringToTable,
        F_JSONB_ARRAY_ELEMENTS => SrfKind::JsonbArrayElements { text: false },
        F_JSONB_ARRAY_ELEMENTS_TEXT => SrfKind::JsonbArrayElements { text: true },
        F_JSONB_EACH => SrfKind::JsonbEach { text: false },
        F_JSONB_EACH_TEXT => SrfKind::JsonbEach { text: true },
        F_JSONB_OBJECT_KEYS => SrfKind::JsonbObjectKeys,
        F_JSON_ARRAY_ELEMENTS => SrfKind::JsonArrayElements { text: false },
        F_JSON_ARRAY_ELEMENTS_TEXT => SrfKind::JsonArrayElements { text: true },
        F_JSON_EACH => SrfKind::JsonEach { text: false },
        F_JSON_EACH_TEXT => SrfKind::JsonEach { text: true },
        F_JSON_OBJECT_KEYS => SrfKind::JsonObjectKeys,
        F_JSONB_PATH_QUERY | F_JSONB_PATH_QUERY_TZ => SrfKind::JsonbPathQuery,
        _ if retset => SrfKind::Other,
        _ => SrfKind::Scalar,
    }
}

// fmgroids.h's names of the builtin directory listings (PostgreSQL 18.6 pg_proc.dat, genfile.c / misc.c): each returns the
// entries of one server directory, so its rows end with that directory (D4546: pg_rewind's file-list query lists the data
// directory with pg_ls_dir in its recursive union's seed)
pub const F_PG_LS_DIR_TEXT: u32 = 2625;
pub const F_PG_LS_DIR_TEXT_BOOL_BOOL: u32 = 3297;
pub const F_PG_LS_LOGDIR: u32 = 3353;
pub const F_PG_LS_WALDIR: u32 = 3354;
pub const F_PG_LS_SUMMARIESDIR: u32 = 6400;
pub const F_PG_LS_ARCHIVE_STATUSDIR: u32 = 5031;
pub const F_PG_LS_TMPDIR_: u32 = 5029;
pub const F_PG_LS_TMPDIR_OID: u32 = 5030;
pub const F_PG_LS_LOGICALSNAPDIR: u32 = 6270;
pub const F_PG_LS_LOGICALMAPDIR: u32 = 6271;
pub const F_PG_LS_REPLSLOTDIR: u32 = 6272;

/// Every directory listing above with its OID (tests/srf_dispatch_names.rs checks each against pg_proc.dat).
pub const LISTINGS: &[(&str, u32)] = &[
    ("F_PG_LS_DIR_TEXT", F_PG_LS_DIR_TEXT),
    ("F_PG_LS_DIR_TEXT_BOOL_BOOL", F_PG_LS_DIR_TEXT_BOOL_BOOL),
    ("F_PG_LS_LOGDIR", F_PG_LS_LOGDIR),
    ("F_PG_LS_WALDIR", F_PG_LS_WALDIR),
    ("F_PG_LS_SUMMARIESDIR", F_PG_LS_SUMMARIESDIR),
    ("F_PG_LS_ARCHIVE_STATUSDIR", F_PG_LS_ARCHIVE_STATUSDIR),
    ("F_PG_LS_TMPDIR_", F_PG_LS_TMPDIR_),
    ("F_PG_LS_TMPDIR_OID", F_PG_LS_TMPDIR_OID),
    ("F_PG_LS_LOGICALSNAPDIR", F_PG_LS_LOGICALSNAPDIR),
    ("F_PG_LS_LOGICALMAPDIR", F_PG_LS_LOGICALMAPDIR),
    ("F_PG_LS_REPLSLOTDIR", F_PG_LS_REPLSLOTDIR),
];

/// A call of `funcid` lists one server directory (`LISTINGS`): its rows end with the directory's entries.
pub const fn lists_a_directory(funcid: u32) -> bool {
    matches!(
        funcid,
        F_PG_LS_DIR_TEXT
            | F_PG_LS_DIR_TEXT_BOOL_BOOL
            | F_PG_LS_LOGDIR
            | F_PG_LS_WALDIR
            | F_PG_LS_SUMMARIESDIR
            | F_PG_LS_ARCHIVE_STATUSDIR
            | F_PG_LS_TMPDIR_
            | F_PG_LS_TMPDIR_OID
            | F_PG_LS_LOGICALSNAPDIR
            | F_PG_LS_LOGICALMAPDIR
            | F_PG_LS_REPLSLOTDIR
    )
}

/// pg_proc.oid of pg_lock_status() (pg_proc.dat), the pg_locks view's FROM call.
pub const PG_LOCK_STATUS: u32 = 1371;

/// A call of `funcid` is pg_lock_status().
pub const fn is_lock_status(funcid: u32) -> bool {
    funcid == PG_LOCK_STATUS
}

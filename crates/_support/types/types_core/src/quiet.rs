//! Raise-free builtins beyond `pg_proc.proleakproof` (D454): the one home of the allowlists that let a qual run ahead
//! of, or be skipped behind, another as PostgreSQL's order would not (the planner's gravity §3 #7 qual order, lowerv4's
//! pgrc verdicts, runs/topk_groups.rs's pure entries). Each entry above the fact tables raises on no input of its
//! argument types.
//!
//! C1 (one effects answer): this file is also the one table of the other builtin facts the effects answer
//! reads beside the catalog's columns (`decompose::effects::procfacts`, generated from pg_proc.dat): calls that act
//! though not VOLATILE (`NOTICE`), calls that run user code named at run time (`RUNS_OWN_QUERY`, `INPUT_VALIDATORS`),
//! calls that run a statement of their own over the catalogs (`CATALOG_QUERY`),
//! VOLATILE calls whose act stays inside the call and the calls that read or set the state they move (`CALL_LOCAL_ACT`,
//! `CALL_LOCAL_STATE`), calls that reach a component type's function through the type cache or a run-time name
//! (`DISPATCH`), the procs records
//! and enums key by (`RECORD_KEY_PROCS`, `ENUM_KEY_PROCS`), raise-free reads (`ARRAY_BOUND`, `HASH_NEEDS_COLLATION` under
//! a set collation, `COLLATION_FREE` under any), order (`ORDERED_COMBINES`, `PEER_ORDER_FREE`) and session-free text
//! (`GUC_FREE_OUT`). Those tables are rows of `Fact`, each naming where PostgreSQL 18.6 shows it (`TABLES` and
//! `DISPATCH`; pinned by decompose tests/effects_home.rs). A builtin list of such facts anywhere else is a second answer.

/// `numeric_eq / ne / gt / ge / lt / le`: PostgreSQL does not mark them leakproof, but a comparison of two numerics
/// (NaN included) raises nothing.
pub const fn numeric_cmp(funcid: u32) -> bool {
    matches!(funcid, 1718..=1723)
}

/// `numeric_cmp` (pg_proc 1769), numeric's btree support function: the same comparison as `numeric_eq .. le` (NaN
/// included), so a sort, a merge or an ordered-set final comparing numerics raises nothing (D1520: agg_shop-0137's
/// hypothetical rank over a numeric). Apart from `numeric_cmp` (an operator's function never names it).
pub const NUMERIC_BTCMP: u32 = 1769;

/// The int4 / int8 / int84 / int48 comparisons (`=`, `<>`, `<`, `>`, `<=`, `>=`; pg_proc oids): leakproof in the catalog;
/// listed for a reader without one (runs/topk_groups.rs D449 `pure`).
pub const INT_CMP: [u32; 24] = [65, 144, 66, 147, 149, 150, 467, 468, 469, 470, 471, 472, 474, 475, 476, 477, 478, 479, 852, 853, 854, 855, 856, 857];

pub fn int_cmp(funcid: u32) -> bool {
    INT_CMP.contains(&funcid)
}

/// `int2mod / int4mod / int8mod` (pg_proc 155, 156, 945: the `%` operators' functions) and `mod(int2, int2)`,
/// `mod(int4, int4)`, `mod(int8, int8)` (pg_proc 940, 941, 947, whose prosrc is the same int2mod / int4mod / int8mod):
/// raise only on a zero divisor (int.c / int8.c return 0 for a -1 one, never overflowing), so `x % c` and `mod(x, c)` by a
/// nonzero constant raise nothing (D567: D454's quiet gate over `k % 7 = 3`).
pub const fn int_mod(funcid: u32) -> bool {
    matches!(funcid, 155 | 156 | 945 | 940 | 941 | 947)
}

/// `x % divisor` under `int_mod` raises nothing: `divisor` is a non-null constant (`None` = NULL or not a constant) other
/// than 0. A -1 divisor is quiet too: PostgreSQL returns 0 for it rather than overflowing on INT_MIN (int.c int4mod /
/// int2mod, int8.c int8mod).
pub const fn int_mod_quiet(funcid: u32, divisor: Option<i64>) -> bool {
    int_mod(funcid) && matches!(divisor, Some(d) if d != 0)
}

/// `int2div / int4div / int8div` and the cross-type int24div / int42div / int48div / int84div / int28div / int82div
/// (pg_proc 153, 154, 466, 172, 173, 1281, 1277, 948, 840): each raises on a zero divisor, and int2div / int4div /
/// int8div / int42div / int84div / int82div also on a -1 one over the dividend type's minimum (int.c / int8.c: "smallint
/// out of range", "integer out of range", "bigint out of range"). The others cannot overflow.
pub const fn int_div(funcid: u32) -> bool {
    matches!(funcid, 153 | 154 | 466 | 172 | 173 | 1281 | 1277 | 948 | 840)
}

/// `x / divisor` under `int_div` raises nothing (div_by_const, main 2026-10-03 19:43Z): `divisor` is a non-null constant
/// (`None` = NULL or not a constant) other than 0 and -1. -1 is declined for every member, the cross-type ones that
/// cannot overflow too.
pub const fn int_div_quiet(funcid: u32, divisor: Option<i64>) -> bool {
    int_div(funcid) && matches!(divisor, Some(d) if d != 0 && d != -1)
}

/// `substr(text, int4, int4)` / `substring(text from int4 for int4)` (pg_proc 877, 936: varlena.c `text_substr`): its one
/// raise is a negative length ("negative substring length not allowed"); any start, however far out, returns a (maybe
/// empty) string. So a call whose `length` is a non-null constant >= 0 raises on no input (main's rule-4 consent, ruling
/// log 2026-10-04 19:36:20Z). Whoever reads the constant to decide a program keys its class (lower key.rs `arg_quiet`).
pub const fn substr_quiet(funcid: u32, length: Option<i64>) -> bool {
    matches!(funcid, 877 | 936) && matches!(length, Some(n) if n >= 0)
}

/// `left / right(text, int4)` (pg_proc 3060, 3061: varlena.c `text_left` / `text_right`): a negative `n` counts from the
/// other end, any `n` past the length returns the whole string; the one value whose negation overflows is INT32_MIN. So
/// a call whose `n` is a non-null constant other than INT32_MIN raises on no input (main's rule-4 consent, ruling log
/// 2026-10-04 19:36:20Z). Keyed as `substr_quiet` (lower key.rs `arg_quiet`).
pub const fn left_right_quiet(funcid: u32, n: Option<i64>) -> bool {
    matches!(funcid, 3060 | 3061) && matches!(n, Some(n) if n != i32::MIN as i64)
}

/// The argument a `substr_quiet` / `left_right_quiet` call's class reads (its length, its n) for `funcid` over `nargs`
/// arguments, none for any other call: the one map lower key.rs `arg_quiet` and effects `quiet_args` share.
pub const fn const_arg(funcid: u32, nargs: usize) -> Option<usize> {
    match (funcid, nargs) {
        (3060 | 3061, 2) => Some(1),
        (877 | 936, 3) => Some(2),
        _ => None,
    }
}

/// `substr_quiet` or `left_right_quiet` over the constant `const_arg` names (its word; `None`: NULL or not a constant).
pub const fn text_arg_quiet(funcid: u32, word: Option<i64>) -> bool {
    substr_quiet(funcid, word) || left_right_quiet(funcid, word)
}

/// D3041: a datetime field unit as PostgreSQL 18.6 decodes it (datetime.c `DecodeUnits` over `deltatktbl`, then
/// `DecodeSpecial` over `datetktbl` for the part functions): the fields the part / trunc functions below accept, `Other`
/// for every other token (`@`, `ago`, a reserved word but `epoch`, a month name, a zone abbreviation) or none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DtUnit {
    Usec,
    Msec,
    Sec,
    Min,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
    Tz,
    TzHour,
    TzMin,
    Julian,
    IsoYear,
    Dow,
    IsoDow,
    Doy,
    Epoch,
    Other,
}

/// datetime.c's `deltatktbl` (DecodeUnits), every row: `@` and `ago` are tokens of other types (`Other`).
const DELTA_UNITS: &[(&str, DtUnit)] = {
    use DtUnit::*;
    &[
        ("@", Other), ("ago", Other), ("c", Century), ("cent", Century), ("centuries", Century), ("century", Century), ("d", Day), ("day", Day),
        ("days", Day), ("dec", Decade), ("decade", Decade), ("decades", Decade), ("decs", Decade), ("h", Hour), ("hour", Hour), ("hours", Hour),
        ("hr", Hour), ("hrs", Hour), ("m", Min), ("microsecon", Usec), ("mil", Millennium), ("millennia", Millennium), ("millennium", Millennium),
        ("millisecon", Msec), ("mils", Millennium), ("min", Min), ("mins", Min), ("minute", Min), ("minutes", Min), ("mon", Month), ("mons", Month),
        ("month", Month), ("months", Month), ("ms", Msec), ("msec", Msec), ("msecond", Msec), ("mseconds", Msec), ("msecs", Msec), ("qtr", Quarter),
        ("quarter", Quarter), ("s", Sec), ("sec", Sec), ("second", Sec), ("seconds", Sec), ("secs", Sec), ("timezone", Tz), ("timezone_h", TzHour),
        ("timezone_m", TzMin), ("us", Usec), ("usec", Usec), ("usecond", Usec), ("useconds", Usec), ("usecs", Usec), ("w", Week), ("week", Week),
        ("weeks", Week), ("y", Year), ("year", Year), ("years", Year), ("yr", Year), ("yrs", Year),
    ]
};

/// datetime.c's `datetktbl` rows of type UNITS, and RESERV `epoch`, that `deltatktbl` does not shadow (its `d`, `h`, `m`,
/// `s`, `y` win in DecodeUnits). Any other `datetktbl` row is a token of another type, `Other` by absence.
const SPECIAL_UNITS: &[(&str, DtUnit)] = {
    use DtUnit::*;
    &[("dow", Dow), ("doy", Doy), ("epoch", Epoch), ("isodow", IsoDow), ("isoyear", IsoYear), ("j", Julian), ("jd", Julian), ("julian", Julian), ("mm", Min)]
};

/// datetime.c `datebsearch`'s match: `strncmp(key, token, TOKMAXLEN)` over NUL-terminated strings, TOKMAXLEN 10.
/// `key` is the unit's own bytes: downcasing them first is ASCII-case-insensitive equality with a lowercase token.
fn token_matches(key: &[u8], token: &str) -> bool {
    let t = token.as_bytes();
    if t.len() < 10 { key.eq_ignore_ascii_case(t) } else { key.len() >= 10 && key[..10].eq_ignore_ascii_case(&t[..10]) }
}

/// `unit` (a unit text's bytes) decoded as PostgreSQL 18.6 does: downcased (downcase_truncate_identifier: ASCII A-Z; a
/// high-bit byte matches no token either way), `deltatktbl` first, then `datetktbl` when `special` (the part functions;
/// date_trunc reads DecodeUnits alone). `None`: no table names it (UNKNOWN_FIELD).
pub fn dt_unit(unit: &[u8], special: bool) -> Option<DtUnit> {
    let find = |tbl: &[(&str, DtUnit)]| tbl.iter().find(|(t, _)| token_matches(unit, t)).map(|e| e.1);
    find(DELTA_UNITS).or_else(|| special.then(|| find(SPECIAL_UNITS)).flatten())
}

/// D3041: extract / date_part / date_trunc over date, timestamp and timestamptz, by pg_proc oid: 6199 extract(text, date),
/// 6202 extract(text, timestamp), 2021 date_part(text, timestamp), 6203 extract(text, timestamptz), 1171 date_part(text,
/// timestamptz), 2020 date_trunc(text, timestamp), 1217 date_trunc(text, timestamptz). date_part(text, date) is a SQL
/// function the planner inlines into date_part(text, timestamp) over a cast.
pub const fn datetime_unit_fn(funcid: u32) -> bool {
    matches!(funcid, 6199 | 6202 | 2021 | 6203 | 1171 | 2020 | 1217)
}

/// D3041: a `datetime_unit_fn` call whose constant unit is `unit` raises on no input of its argument types, infinities
/// included: `Some(true)` for the fields PostgreSQL 18.6's code computes without a raise for every value of the type
/// (timestamp.c timestamp_part_common / timestamptz_part_common and NonFiniteTimestampTzPart, date.c extract_date,
/// timestamp.c timestamp_trunc / timestamptz_trunc_internal); `Some(false)` for any other unit (not recognized, not
/// supported for the type, and the ones that raise on some value); `None` for another function.
/// - the part functions over timestamp: microseconds .. millennium, julian, isoyear, dow, isodow, doy and epoch; the
///   timezone fields raise "not supported" on a finite timestamp (though an infinite one yields NULL for them).
/// - over timestamptz: those and timezone, timezone_hour, timezone_minute.
/// - over date: day, week, month, quarter, year .. millennium, julian, isoyear, dow, isodow, doy, epoch (no time fields).
/// - date_trunc(text, timestamp): day and finer only. A coarser field truncates below the earliest timestamp (4714-11-24
///   BC) and raises "timestamp out of range" there (tm2timestamp); the earliest is a midnight, so day and finer cannot.
/// - date_trunc(text, timestamptz): second and finer only. The local day, hour and minute starts can fall below the
///   earliest UTC instant under a zone whose offset is not a whole hour or minute (an LMT offset), and day and coarser
///   redo the zone offset (DetermineTimeZoneOffset); a second boundary moves by whole seconds, which no offset breaks.
pub fn datetime_unit_quiet(funcid: u32, unit: &[u8]) -> Option<bool> {
    use DtUnit::*;
    if !datetime_unit_fn(funcid) {
        return None;
    }
    let trunc = matches!(funcid, 2020 | 1217);
    let Some(u) = dt_unit(unit, !trunc) else { return Some(false) };
    let common = matches!(u, Year | Decade | Century | Millennium | Julian | IsoYear | Dow | IsoDow | Doy | Epoch | Day | Week | Month | Quarter);
    let time = matches!(u, Usec | Msec | Sec | Min | Hour);
    Some(match funcid {
        6199 => common,
        6202 | 2021 => common || time,
        6203 | 1171 => common || time || matches!(u, Tz | TzHour | TzMin),
        2020 => matches!(u, Usec | Msec | Sec | Min | Hour | Day),
        _ => matches!(u, Usec | Msec | Sec),
    })
}

/// `max(text)` / `min(text)` (pg_proc 2129, 2145; transitions `text_larger` / `text_smaller`): a text comparison raises on
/// no input under a deterministic collation the server compares bytewise or by its default: the default, C or POSIX
/// (`collid` 100, 950, 951). An input collation of 0 is a conflict of implicit collations the parser left to run time
/// (parse_collate.c), and varstr_cmp's check_collation_set raises 42P22 at the first comparison: not quiet.
pub const fn text_extremum_quiet(aggfnoid: u32, collid: u32) -> bool {
    matches!(aggfnoid, 2129 | 2145) && matches!(collid, 100 | 950 | 951)
}

/// `lower / upper / initcap (text)` (pg_proc 870, 871, 872: oracle_compat.c over formatting.c `str_tolower` /
/// `str_toupper` / `str_initcap`) under input collation `collid`. PostgreSQL 18.6 raises in them on an unset collation
/// (42P22, "could not determine which collation to use"), in the collation's locale lookup
/// (pg_newlocale_from_collation's catalog read), in a provider's case mapping (libc's char2wchar on a string its
/// LC_CTYPE cannot read, ICU's "case conversion failed"), and on allocation. Under the database default (100), C (950)
/// or POSIX (951) only resource limits are left: the default's locale is the backend's own and C's is `c_locale` (no
/// lookup); POSIX's first call in a backend reads its initdb pg_collation row (a syscache read and newlocale("POSIX"),
/// no collversion so no version WARNING), a lookup that fails only on resource limits; C and POSIX then map ASCII bytes
/// in place (`ctype_is_c`: asc_tolower and its kin), and the default's provider reads every valid string of the server
/// encoding, which is UTF8 only (user ruling 2026-10-07; createdb refuses an LC_CTYPE that does not match it). The
/// allocation and that one catalog read are resource limits, quiet by the user ruling of 2026-10-04 (resource limits
/// are no raise a reading must keep in place). Any other collation (an explicit libc or ICU one, a nondeterministic
/// one) keeps the call raising.
pub const fn case_map_quiet(funcid: u32, collid: u32) -> bool {
    matches!(funcid, 870 | 871 | 872) && matches!(collid, 100 | 950 | 951)
}

/// A plain aggregate whose transitions and final raise on no input (its arguments apart): `count(*)` / `count(any)`
/// (2803, 2147), `extremum_quiet` and `text_extremum_quiet`. The planner's InitPlan proof reads it (D2875);
/// `count(DISTINCT x)` is its caller's, by x's type (`distinct_count_quiet`). D2875's subset rule: the proof is never
/// wider than the effects home (decompose `effects::agg_oid_quiet` reads every `extremum_quiet` min / max quiet, timetz
/// and interval too: NEW-44; a decompose unit test holds the two equal), or it orders a comparison first that lowerv4's
/// pgrcolumnar reading keeps loud and the later quals lose their zone verdicts.
pub const fn agg_quiet(aggfnoid: u32, collid: u32) -> bool {
    matches!(aggfnoid, 2803 | 2147) || extremum_quiet(aggfnoid) || text_extremum_quiet(aggfnoid, collid)
}

/// A type whose own comparison raises on no input (its btree `=` / `<` / cmp, its hash, `min` / `max`'s `*_larger` /
/// `*_smaller`), with no collation: int2, int4, int8, oid, float4, float8 (NaN orders above every number), date, time,
/// timetz, money, timestamp, timestamptz, interval, numeric (NaN included), tid, pg_lsn, xid8 (pg_type oids). So a sort,
/// a hash or a DISTINCT over it raises nothing (count(DISTINCT x), the planner's affine aggregate rewrite, PSP2-11).
/// Text and the other collatable types compare under a collation (`text_extremum_quiet`); arrays, records, enums,
/// bpchar, inet, bytea and user types are not listed.
pub const fn compare_quiet(typoid: u32) -> bool {
    matches!(typoid, 20 | 21 | 23 | 26 | 27 | 700 | 701 | 790 | 1082 | 1083 | 1114 | 1184 | 1186 | 1266 | 1700 | 3220 | 5069)
}

/// `count(DISTINCT x)` (`count(any)`, pg_proc 2147) over a `compare_quiet` type `argtype`: the sort or hash that drops
/// the duplicates raises nothing.
pub const fn distinct_count_quiet(aggfnoid: u32, argtype: u32) -> bool {
    aggfnoid == 2147 && compare_quiet(argtype)
}

/// Builtin `min` / `max` aggregate `aggfnoid` (pg_proc oids, PostgreSQL 18.6's pg_aggregate.dat) over a `compare_quiet`
/// type: its transition and final raise on no input. min / max of text: `text_extremum_quiet`.
pub const fn extremum_quiet(aggfnoid: u32) -> bool {
    let t = match aggfnoid {
        2115 | 2131 => 20,
        2116 | 2132 => 23,
        2117 | 2133 => 21,
        2118 | 2134 => 26,
        2119 | 2135 => 700,
        2120 | 2136 => 701,
        2122 | 2138 => 1082,
        2123 | 2139 => 1083,
        2124 | 2140 => 1266,
        2125 | 2141 => 790,
        2126 | 2142 => 1114,
        2127 | 2143 => 1184,
        2128 | 2144 => 1186,
        2130 | 2146 => 1700,
        2797 | 2798 => 27,
        4189 | 4190 => 3220,
        5099 | 5100 => 5069,
        _ => return false,
    };
    compare_quiet(t)
}

/// An integer `%` or `/` call's divisor class: `Some(quiet)` for `int_mod` (`int_mod_quiet`) and `int_div`
/// (`int_div_quiet`), `None` for any other function. Whoever reads it to decide a program keys it (lower key.rs
/// `mod_quiet`, D1103).
pub const fn divisor_quiet(funcid: u32, divisor: Option<i64>) -> Option<bool> {
    match (int_mod(funcid), int_div(funcid)) {
        (true, _) => Some(int_mod_quiet(funcid, divisor)),
        (_, true) => Some(int_div_quiet(funcid, divisor)),
        _ => None,
    }
}

/// AGB-20: an integer `+`, `-`, `*` or unary `-` (`int_arith`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntOp {
    Add,
    Sub,
    Mul,
    Neg,
}

const I2: (i128, i128) = (i16::MIN as i128, i16::MAX as i128);
const I4: (i128, i128) = (i32::MIN as i128, i32::MAX as i128);
const I8: (i128, i128) = (i64::MIN as i128, i64::MAX as i128);

/// AGB-20: integer `+`, `-`, `*` and unary `-` over int2 / int4 / int8, the cross-type ones too (pg_proc 176-183, 141,
/// 152, 170, 171, 212, 213, 462-465, 837-839, 841, 942, 943, 1274-1276, 1278-1280: int.c / int8.c): each raises only
/// when its exact result leaves its result type ("smallint / integer / bigint out of range"). The operation and the
/// result type's range; none for any other function.
pub const fn int_arith(funcid: u32) -> Option<(IntOp, (i128, i128))> {
    use IntOp::*;
    Some(match funcid {
        176 => (Add, I2),
        180 => (Sub, I2),
        152 => (Mul, I2),
        213 => (Neg, I2),
        177..=179 => (Add, I4),
        181..=183 => (Sub, I4),
        141 | 170 | 171 => (Mul, I4),
        212 => (Neg, I4),
        463 | 1274 | 1278 | 837 | 841 => (Add, I8),
        464 | 1275 | 1279 | 838 | 942 => (Sub, I8),
        465 | 1276 | 1280 | 839 | 943 => (Mul, I8),
        462 => (Neg, I8),
        _ => return None,
    })
}

/// The widening integer casts int4(int2), int8(int4) and int8(int2) (pg_proc 313, 481, 754: i2toi4, int48, int28; int.c
/// i2toi4 and int8.c int48 / int28 return `(int32) arg` / `(int64) arg`, no ereport; `pg_cast.dat` method `f`): the
/// value unchanged, no raise, so the order is kept too: `cast(x) op c` holds exactly when the sign-extended word of `x`
/// compares with `c` as `op` does (a zone predicate reads the lane through it). C1: the one list (decompose catalog
/// casts.rs `int_widening` was its copy).
pub const fn int_widen(funcid: u32) -> bool {
    matches!(funcid, 313 | 481 | 754)
}

/// D3712: the result range of a builtin integer btree comparator (nbtcompare.c): btint4cmp, btoidcmp, btint8cmp, the
/// cross-type int comparators (pg_proc 351, 356, 842, 2188-2193) return -1, 0 or 1, btboolcmp (1693) a difference of two
/// bools, btint2cmp (350) of two int2, btcharcmp (358) of two unsigned chars. None for any other function.
pub const fn cmp_range(funcid: u32) -> Option<(i128, i128)> {
    match funcid {
        351 | 356 | 842 | 1693 | 2188..=2193 => Some((-1, 1)),
        350 => Some((-65535, 65535)),
        358 => Some((-255, 255)),
        _ => None,
    }
}

/// The range of an int2, int4 or int8 (`typid` 21, 23, 20); none for any other type (a domain over one included).
pub const fn int_type_range(typid: u32) -> Option<(i128, i128)> {
    match typid {
        21 => Some(I2),
        23 => Some(I4),
        20 => Some(I8),
        _ => None,
    }
}

/// AGB-20: the exact range of `op` over an operand in `a` and, but for `Neg`, one in `b` (interval arithmetic; i128
/// holds every int8 product), before the result type's check.
pub const fn int_arith_range(op: IntOp, a: (i128, i128), b: (i128, i128)) -> (i128, i128) {
    match op {
        IntOp::Add => (a.0 + b.0, a.1 + b.1),
        IntOp::Sub => (a.0 - b.1, a.1 - b.0),
        IntOp::Neg => (-a.1, -a.0),
        IntOp::Mul => {
            let p = [a.0 * b.0, a.0 * b.1, a.1 * b.0, a.1 * b.1];
            let (mut lo, mut hi, mut i) = (p[0], p[0], 1);
            while i < 4 {
                if p[i] < lo {
                    lo = p[i];
                }
                if p[i] > hi {
                    hi = p[i];
                }
                i += 1;
            }
            (lo, hi)
        }
    }
}

/// AGB-20: `op` over operands in ranges `a` and `b` keeps its exact result inside the result type's range `ty`
/// (`int_arith`), so it raises on no input.
pub const fn int_arith_quiet(op: IntOp, ty: (i128, i128), a: (i128, i128), b: (i128, i128)) -> bool {
    let r = int_arith_range(op, a, b);
    r.0 >= ty.0 && r.1 <= ty.1
}

/// AGB-19: numeric arithmetic and the exact casts into numeric, by how each bounds its result's integer digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumOp {
    /// `+` / `-` (numeric_add, numeric_sub): the wider operand's digits and one more.
    AddSub,
    /// `*` (numeric_mul): the sum of the operands' digits.
    Mul,
    /// unary `-` (numeric_uminus): the operand's digits.
    Neg,
    /// numeric(int2 / int4 / int8): the integer's digits; never raises.
    Cast,
    Integral,
}

/// AGB-19: numeric operation `funcid` and its arity (1724 / 1725 add / sub, 1726 mul, 1771 uminus, 1782 / 1740 / 1781
/// numeric(int2 / int4 / int8), D4021's `Integral` 1712 floor, 1711 / 2167 ceil: the digits and a carry); none else.
pub const fn num_arith(funcid: u32) -> Option<(NumOp, usize)> {
    Some(match funcid {
        1724 | 1725 => (NumOp::AddSub, 2),
        1726 => (NumOp::Mul, 2),
        1771 => (NumOp::Neg, 1),
        1782 | 1740 | 1781 => (NumOp::Cast, 1),
        1712 | 1711 | 2167 => (NumOp::Integral, 1),
        _ => return None,
    })
}

/// AGB-19: the integer digits of an int2, int4 or int8 (`typid` 21, 23, 20: |v| < 10^5, 10^10, 10^19); none for any
/// other type.
pub const fn int_digits(typid: u32) -> Option<u32> {
    match typid {
        21 => Some(5),
        23 => Some(10),
        20 => Some(19),
        _ => None,
    }
}

/// A by-value integer constant's value: the low `len` bytes (2, 4 or 8) of its word, sign-extended, as int.c's
/// PG_GETARG_INT16 / INT32 / INT64 read it. `None` for any other length.
pub const fn int_word(word: usize, len: i32) -> Option<i64> {
    match len {
        2 => Some(word as i16 as i64),
        4 => Some(word as i32 as i64),
        8 => Some(word as i64),
        _ => None,
    }
}

/// `jsonb -> text`, `jsonb ->> text`, `jsonb -> int4`, `jsonb ->> int4` (pg_proc 3478, 3214, 3215, 3216): PostgreSQL does not
/// mark them leakproof, but on any jsonb they return the field / element or NULL (a scalar or the other container kind,
/// a missing key, an index out of range, negative included) and raise nothing (jsonfuncs.c jsonb_object_field* /
/// jsonb_array_element*). M6-REPS (M6-JSONB R0): a `j ->> 'k' = c` lead qual keeps the pgrc condition cache, bloom
/// and zone verdicts (C4 / C12).
pub const fn jsonb_field(funcid: u32) -> bool {
    matches!(funcid, 3214 | 3215 | 3216 | 3478)
}

/// D2212 (Hibernate's jsonb_mergepatch emulation): `jsonb_typeof(jsonb)` (pg_proc 3210; jsonb.c JsonbContainerTypeName,
/// whose one ereport is a container type no stored jsonb holds) and `cardinality(anyarray)` (pg_proc 3179; arrayfuncs.c
/// array_cardinality: ArrayGetNItems over a stored array's own dimensions, which its construction already bounded) raise
/// on no input. PostgreSQL does not mark them leakproof.
pub const fn shape_read(funcid: u32) -> bool {
    matches!(funcid, 3210 | 3179)
}

/// D3847: `array_eq` / `array_ne` (pg_proc 744 / 390) over two arguments of one listed built-in array type
/// (`eq_alike::array_elem`) raise on no input. arrayfuncs.c array_eq raises only on two element types apart (one declared
/// array type holds one element type) or on an element type with no default equality; it compares dimensions and lower
/// bounds before any element, and each listed element type's `=` raises nothing under a set collation (decompose
/// `effects::collation_set` asks the call's).
pub fn array_eq_quiet(funcid: u32, left: u32, right: u32) -> bool {
    matches!(funcid, 744 | 390) && left == right && crate::eq_alike::array_elem(left).is_some()
}

/// `generate_series(int4, int4)` and `(int8, int8)` (pg_proc 1067, 1069): no step to be zero, and they stop before an
/// overflow (int.c generate_series_int4 / int8.c generate_series_int8), so they raise on no input.
pub const fn int_series(funcid: u32) -> bool {
    matches!(funcid, 1067 | 1069)
}

/// `unnest(anyarray)` (pg_proc 2331, arrayfuncs.c array_unnest): one row per element of a deconstructed array, none for
/// an empty one (a NULL array: a strict function's NULL), so it raises on no input.
pub const fn array_unnest(funcid: u32) -> bool {
    funcid == 2331
}

/// `generate_subscripts(anyarray, int4 [, bool])` (pg_proc 1191, 1192; arrayfuncs.c generate_subscripts): the subscripts
/// of one dimension, none for a dimension outside the array's (zero, negative, past its last) and none for an empty array
/// (a NULL argument: a strict function's NULL). The bounds ArrayCheckBounds admitted end below INT_MAX, so it raises on
/// no input.
pub const fn array_subscripts(funcid: u32) -> bool {
    matches!(funcid, 1191 | 1192)
}

/// The start times `now()`, `transaction_timestamp()` and `statement_timestamp()` (pg_proc 1299, 2647, 2648; main's
/// ruling (6) for join#132): no argument; timestamp.c returns the transaction's or the statement's start
/// (GetCurrentTransactionStartTimestamp, GetCurrentStatementStartTimestamp) with no ereport. Their STABLE volatility is the
/// catalog's, apart from this answer. clock_timestamp() and timeofday() are not listed.
pub const fn start_time(funcid: u32) -> bool {
    matches!(funcid, 1299 | 2647 | 2648)
}

/// D3882 (pgwatch's `epoch_ns` column, a delegated ruling): `(extract(epoch FROM <clock>) *
/// k)::int8`, `<clock>` one of now(), transaction_timestamp(), statement_timestamp() (`start_time`) or CURRENT_TIMESTAMP
/// [(n)] (`epoch_clock_value`), `k` a non-null numeric constant with |k| <= 1e9 (`EPOCH_NS_MAX_K`), raises on no input.
/// The proof: the clock is the transaction's or the statement's start (timestamp.c GetCurrentTransactionStartTimestamp /
/// GetCurrentStatementStartTimestamp, xact.c sets them from GetCurrentTimestamp, gettimeofday), a finite timestamptz
/// (never -infinity / infinity), which the ruling reads as after 1677-09-21 and before 2262-04-11, the range whose epoch
/// in nanoseconds fits an int8 (CURRENT_TIMESTAMP(n) rounds that instant to n fractional digits, AdjustTimestampForTypmod,
/// at most half a second away). extract(epoch FROM t) over a finite timestamptz is the exact numeric seconds since
/// 1970-01-01 (timestamptz_part_common, unit `epoch`: no raise for any finite or infinite value, `datetime_unit_quiet`), so
/// |extract(epoch FROM t)| < 9223372036.854775807 and |extract(epoch FROM t) * k| < 9223372036854775807: numeric_mul's
/// one raise (a result past numeric's 131072 integer digits) and numeric_int8's (outside int8 after rounding, NaN,
/// infinity) are not reached. Exactly this shape: another multiplier, cast, clock (clock_timestamp() is not listed) or
/// date arithmetic keeps its own reading. The three calls' oids: numeric_int8 (pg_proc 1779, `::int8` of a numeric),
/// numeric_mul (1726, numeric `*`), extract(text, timestamptz) (6203).
pub const EPOCH_NS_CAST: u32 = 1779;
pub const EPOCH_NS_MUL: u32 = 1726;
pub const EPOCH_NS_EXTRACT: u32 = 6203;
pub const EPOCH_NS_MAX_K: i128 = 1_000_000_000;

/// D3882: the SQLValueFunctions of `epoch_ns_quiet`'s clock, CURRENT_TIMESTAMP and CURRENT_TIMESTAMP(n)
/// (`SQLValueFunctionOp` 3, 4: timestamp.c GetSQLCurrentTimestamp, the transaction's start).
pub const fn epoch_clock_value(op: u32) -> bool {
    matches!(op, 3 | 4)
}

/// D3882: whether a numeric's on-disk image (`payload`: the varlena's data, numeric.c `NumericData` in the server's
/// native, little-endian, byte order) is a finite value with |v| <= `EPOCH_NS_MAX_K`. The short form (`NUMERIC_SHORT`,
/// header 0x8000) keeps the weight in its header's low 7 bits (0x0040 its sign), the long form (header 0x0000 / 0x4000)
/// in the int16 after it; the base-10000 digits follow. NaN and the infinities (header 0xC000 / 0xD000 / 0xF000) are not.
pub fn epoch_ns_k_quiet(payload: &[u8]) -> bool {
    let word = |i: usize| payload.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let Some(h) = word(0) else { return false };
    let (weight, at) = match h & 0xC000 {
        0x8000 => (if h & 0x0040 != 0 { (h & 0x003F) as i32 - 64 } else { (h & 0x003F) as i32 }, 2),
        0x0000 | 0x4000 => match word(2) {
            Some(w) => (w as i16 as i32, 4),
            None => return false,
        },
        _ => return false,
    };
    if (payload.len() - at) % 2 != 0 {
        return false;
    }
    let (mut int, mut frac) = (0i128, false);
    for (i, d) in payload[at..].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).enumerate() {
        let e = weight - i as i32;
        if d >= 10_000 {
            return false;
        }
        match e {
            _ if d == 0 => {}
            3.. => return false,
            0..=2 => int += i128::from(d) * 10_000i128.pow(e as u32),
            _ => frac = true,
        }
    }
    int < EPOCH_NS_MAX_K || int == EPOCH_NS_MAX_K && !frac
}

/// The datetime SQLValueFunctions (primnodes.h `SQLValueFunctionOp` 0..=8: CURRENT_DATE, CURRENT_TIME[(n)],
/// CURRENT_TIMESTAMP[(n)], LOCALTIME[(n)], LOCALTIMESTAMP[(n)]): the transaction's start time converted to the result
/// type (date.c GetSQLCurrentDate / GetSQLCurrentTime / GetSQLLocalTime, timestamp.c GetSQLCurrentTimestamp /
/// GetSQLLocalTimestamp). Their only ereports are a start time out of the type's range and a typmod the grammar has
/// already checked, so they raise on no input. The role, catalog and schema ops (9..=14) are not listed.
pub const fn start_time_value(op: u32) -> bool {
    op <= 8
}

/// The SQLValueFunctions that raise on no input, the one answer of decompose's expression reader (`quiet_node`) and its F5
/// walk (D3920): the datetime ops (`start_time_value`), CURRENT_CATALOG (13: name.c current_database, the connected
/// database's name, which cannot be dropped under it) and CURRENT_SCHEMA (14: name.c current_schema, NULL for an empty
/// search path or a namespace dropped meanwhile). The role ops (9..=12: CURRENT_ROLE, CURRENT_USER, USER, SESSION_USER;
/// name.c current_user / session_user, GetUserNameFromId(.., false)) raise 'invalid role OID' once another session
/// dropped the role.
pub const fn svf_raise_free(op: u32) -> bool {
    start_time_value(op) || op == 13 || op == 14
}

/// The integer bitwise operators `&`, `|`, `#` and `~` over int2 / int4 / int8 (pg_proc 1892-1895, 1898-1901,
/// 1904-1907): int.c / int8.c compute them with no check, so they raise on no input (PostgreSQL does not mark them
/// leakproof).
pub const fn int_bitwise(funcid: u32) -> bool {
    matches!(funcid, 1892..=1895 | 1898..=1901 | 1904..=1907)
}

/// The oid forms of `pg_has_role(oid, text)`, `has_table_privilege(oid, text)`, `has_any_column_privilege(oid, text)`,
/// `has_type_privilege(oid, text)` and `has_function_privilege(oid, text)` (pg_proc 2710, 1927, 3029, 3143, 2261), each
/// with the privilege names its acl.c convert_*_priv_string accepts (each also `<name> WITH GRANT OPTION`, a role's also
/// `WITH ADMIN OPTION`). acl.c reads a missing object as NULL (the has_* forms' is_missing) or false (pg_role_aclcheck),
/// so the oid raises nothing; the only raise is convert_any_priv_string's "unrecognized privilege type". The function
/// form is information_schema.routines' and .parameters' filter beside pg_has_role (regress create_function_sql#92).
/// D3754: `has_column_privilege(oid, int2, text)` (pg_proc 3023, information_schema.columns' filter beside pg_has_role):
/// column_privilege_check reads a missing table, column or dropped column as NULL (is_missing), so the oid and the
/// attnum raise nothing; its privilege text is the third argument. The last field: the argument count, the text last.
pub const PRIVILEGES: [(u32, &[&str], bool, usize); 6] = [
    (2710, &["USAGE", "MEMBER", "SET"], true, 2),
    (1927, &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "MAINTAIN"], false, 2),
    (3029, &["SELECT", "INSERT", "UPDATE", "REFERENCES"], false, 2),
    (3143, &["USAGE"], false, 2),
    (2261, &["EXECUTE"], false, 2),
    (3023, &["SELECT", "INSERT", "UPDATE", "REFERENCES"], false, 3),
];

/// Whether `funcid` is one of the `PRIVILEGES` functions. A caller reads the privilege text's image only then: reading a
/// constant's image keys it (decompose `bake::read`), so a text read for any other call would key that call's literal.
pub const fn privilege_fn(funcid: u32) -> bool {
    privilege_args(funcid).is_some()
}

/// A `PRIVILEGES` function's argument count, its privilege text the last argument; `None` for any other function.
pub const fn privilege_args(funcid: u32) -> Option<usize> {
    let mut i = 0;
    while i < PRIVILEGES.len() {
        if PRIVILEGES[i].0 == funcid {
            return Some(PRIVILEGES[i].3);
        }
        i += 1;
    }
    None
}

/// A `PRIVILEGES` call whose privilege text is `privilege` (its bytes): `Some(true)` when acl.c's
/// convert_any_priv_string accepts it (every comma-separated chunk, trimmed of C whitespace, names one of the function's
/// privileges, ASCII case-insensitive as pg_strcasecmp), so the call raises on no input; `Some(false)` when it raises
/// "unrecognized privilege type" (an empty chunk too); `None` for any other function.
pub fn privilege_quiet(funcid: u32, privilege: &[u8]) -> Option<bool> {
    let (_, names, role, _) = PRIVILEGES.iter().find(|p| p.0 == funcid)?;
    let space = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    let known = |chunk: &[u8]| {
        let start = chunk.iter().position(|b| !space(b)).unwrap_or(chunk.len());
        let end = chunk.iter().rposition(|b| !space(b)).map_or(start, |e| e + 1);
        let chunk = &chunk[start..end];
        let options: &[&str] = if *role { &["", " WITH GRANT OPTION", " WITH ADMIN OPTION"] } else { &["", " WITH GRANT OPTION"] };
        names.iter().any(|n| {
            options.iter().any(|o| chunk.len() == n.len() + o.len() && chunk[..n.len()].eq_ignore_ascii_case(n.as_bytes()) && chunk[n.len()..].eq_ignore_ascii_case(o.as_bytes()))
        })
    };
    Some(privilege.split(|&b| b == b',').all(known))
}

/// D3604: `enum_eq` and `enum_ne` (enum.c): compare the two labels' oids, never a raise (pg_proc marks them not leakproof;
/// the ordering ones read the type's sort order through the typcache). River's river_job_state_in_bitmask cases on its
/// state enum.
pub const ENUM_EQ: [u32; 2] = [3508, 3509];

/// D3749: `row_number`, `rank` and `dense_rank` (windowfuncs.c): the row's position or its peer group's, never a raise
/// (the peer test is the window node's own ordering operators, read beside it). River's jobs_with_rownum numbers each
/// unique key's due jobs.
pub const WINDOW_RANK: [u32; 3] = [3100, 3101, 3102];

/// D3749: a window function call of `winfnoid` over `nargs` arguments raises nothing: `WINDOW_RANK`'s, which take none.
/// pg_proc marks them not strict (they read no argument), so the strict test a quiet call passes says nothing here.
pub fn window_quiet(winfnoid: u32, nargs: usize) -> bool {
    nargs == 0 && WINDOW_RANK.contains(&winfnoid)
}

/// D3604: `get_bit(bit, int4)` (varbit.c bitgetbit) raises only for an index outside 0 .. bitlen - 1, and a bit(len) value
/// holds exactly `len` bits (bit_in and the length coercion raise on any other): quiet over a `len` known (its typmod; -1
/// unknown) and an index `n` inside it (`None`: NULL or not a constant). `None` for any other function.
pub fn bit_index(funcid: u32, len: i32, n: Option<i64>) -> Option<bool> {
    const GET_BIT: u32 = 3032;
    (funcid == GET_BIT).then_some(len > 0 && matches!(n, Some(n) if n >= 0 && n < len as i64))
}

/// The `pg_*_is_visible` functions (table, type, function, operator, opclass, opfamily, conversion, statistics object,
/// text search parser / dictionary / template / configuration, collation): NULL for an oid with no object, never a raise.
pub const VISIBLE: [u32; 13] = [2079, 2080, 2081, 2082, 2083, 3829, 2093, 3403, 3756, 3757, 3768, 3758, 3815];

/// `point(float8, float8)` (pg_proc 1440, geo_ops.c construct_point): stores its two arguments in a new Point, never a
/// raise, whatever the floats (NaN and infinities included). D1911: regress create_index#108 / #109's target entry
/// `point(x, x)` before an eager SubPlan.
pub const CONSTRUCT_POINT: u32 = 1440;

/// `pg_my_temp_schema()` and `pg_is_other_temp_schema(oid)` (pg_proc 2854, 2855; DC-14): the backend's temp namespace oid
/// (0 before one exists), and whether an oid names another backend's temp namespace (namespace.c isOtherTempNamespace:
/// a missing namespace is false), never a raise.
pub const TEMP_SCHEMA: [u32; 2] = [2854, 2855];

/// D3860 (main 2026-10-11T01:16:57Z, pgwatch blocking_locks over the pg_stat_activity view): the session reads, each with
/// no ereport reachable for any argument (PG 18.6 source):
/// - `current_database()` (pg_proc 861, misc.c current_database): no argument; namestrcpy of get_database_name(MyDatabaseId),
///   the connected database's name, into a palloc'd Name.
/// - `pg_backend_pid()` (pg_proc 2026, pgstatfuncs.c pg_backend_pid): no argument; PG_RETURN_INT32(MyProcPid).
/// - `pg_stat_get_activity(int4)` (pg_proc 2022, pgstatfuncs.c pg_stat_get_activity): its pid only filters (NULL reads -1,
///   every backend; a pid with no backend returns no row), each backend's row is built from its shared status entry with
///   no ereport, and its two parses read text the server wrote itself (inet_in of getnameinfo's NI_NUMERICHOST address,
///   numeric_in of the TLS serial); InitMaterializedSRF's checks are of the call site (a FROM-clause set function passes).
pub const SESSION: [u32; 3] = [861, 2026, 2022];

/// `text("char")` / `"char"(text)` (pg_proc 946 / 944, char.c char_text / text_char): the byte as a text of no byte, the
/// byte or its four-byte octal escape, and a text's first byte or the byte its octal escape names, never a raise. D3771:
/// SQLAlchemy's foreign key reflection filters `contype = 'f'::VARCHAR` under a Material.
pub const CHAR_TEXT: [u32; 2] = [944, 946];

/// `textcat(text, text)` (pg_proc 1258, the `||` of text): varlena.c text_catenate's one raise is the result's
/// MaxAllocSize, a resource limit; the user's ruling (2026-10-04 ~20:14Z) counts a resource-limit-only raise as none.
pub const TEXTCAT: u32 = 1258;

/// `nameconcatoid(name, oid)` (pg_proc 266, name.c): the name, clipped by pg_mbcliplen to leave room, then "_<oid>" in a
/// new NAMEDATALEN buffer, never a raise. information_schema's specific_name, the hash key of a join of its routine views
/// (regress create_function_sql#92).
pub const NAMECONCATOID: u32 = 266;

/// `jsonb ? text` (pg_proc 4047, jsonb_op.c jsonb_exists): the key looked up as a string among an object's keys or an
/// array's scalars (findJsonbValueFromContainer), true or false on any jsonb, no ereport. Heldout rnd-0426: the filter of
/// each arm of a partitioned table's Append streamed as a nest inner's probe side.
pub const JSONB_EXISTS: u32 = 4047;

/// `jsonb @> jsonb` and `jsonb <@ jsonb` (pg_proc 4046 jsonb_contains, 4050 jsonb_contained, its commutator; jsonb_op.c):
/// false when one root is an object and the other not, else jsonb_util.c JsonbDeepContains, which compares by type (a
/// string by lengthCompareJsonbString's memcmp, a number by numeric_eq, a bool by value) and recurses only into the
/// containers the contained side holds. Its raises are check_stack_depth (a resource limit, which the user's ruling
/// (2026-10-04 ~20:14Z) counts as none) and elog(ERROR) on a container type that cannot occur. D3995 (rnd-3332): the arm
/// filter of a partitioned table's Append streamed as a hash join's probe side.
pub const JSONB_CONTAINS: [u32; 2] = [4046, 4050];

/// D4021: textlen (varlena.c text_length -> mbutils.c pg_mbstrlen_with_len -> pg_mblen_with_len) raises only on a multibyte
/// sequence cut short, which no text holds: each path that makes one verifies it (COPY, parquet, textrecv, literals).
pub const TEXT_LENGTH: [u32; 4] = [1257, 1317, 1369, 1381];

/// D4089: octet_length of text and of bpchar (pg_proc 1374 textoctetlen, varlena.c; 1375 bpcharoctetlen, varchar.c) return
/// toast_raw_datum_size(arg) - VARHDRSZ without detoasting: detoast.c toast_raw_datum_size reads the varlena header or the
/// toast pointer it holds and raises nothing. (rnd-3460's octet_length form: a LEFT JOIN ON over a pgrc copy.)
pub const OCTET_LENGTH: [u32; 2] = [1374, 1375];

/// `information_schema._pg_expandarray(anyarray)` (PG 18.6 information_schema.sql, LANGUAGE sql): its whole body, an unnest
/// with ordinality, raises on no input. Its oid is initdb's and a superuser may CREATE OR REPLACE it, so a caller matches
/// the namespace, the name, the language and the body itself (`sql_body`), read from the catalog at the lowering (a
/// pg_proc change moves the catalog epoch the program cache keys on). Regress create_function_sql#92 and heldout
/// scan-0016: information_schema.parameters' and key_column_usage's target-list set function.
pub const EXPANDARRAY: (&str, &str, &str) = ("information_schema", "_pg_expandarray", "SELECT * FROM pg_catalog.unnest($1) WITH ORDINALITY");

/// pg_language's `sql` (SQLlanguageId).
pub const SQL_LANGUAGE: u32 = 14;

/// A function in namespace `nsp`, named `name`, of language `lang`, whose body is `src`, raises on no input of its argument
/// types: `EXPANDARRAY`'s exact row (a replaced body, another schema or language is not the rule's).
pub fn sql_body(nsp: &[u8], name: &[u8], lang: u32, src: &[u8]) -> bool {
    lang == SQL_LANGUAGE && nsp == EXPANDARRAY.0.as_bytes() && name == EXPANDARRAY.1.as_bytes() && src == EXPANDARRAY.2.as_bytes()
}

/// A call of builtin `funcid` raises on no input of its argument types though `pg_proc` does not mark it leakproof: the
/// one answer the effects home reads beside the catalog (each list above whose rule needs no argument).
pub fn call(funcid: u32) -> bool {
    numeric_cmp(funcid) || funcid == NUMERIC_BTCMP || jsonb_field(funcid) || shape_read(funcid) || int_series(funcid) || array_unnest(funcid) || array_subscripts(funcid) || int_bitwise(funcid) || start_time(funcid) || VISIBLE.contains(&funcid) || ENUM_EQ.contains(&funcid) || funcid == CONSTRUCT_POINT || TEMP_SCHEMA.contains(&funcid) || funcid == TEXTCAT || CHAR_TEXT.contains(&funcid) || SESSION.contains(&funcid) || funcid == NAMECONCATOID || funcid == JSONB_EXISTS || JSONB_CONTAINS.contains(&funcid) || TEXT_LENGTH.contains(&funcid) || OCTET_LENGTH.contains(&funcid)
}

/// D3816: the pinned non-acting builtins, (pg_proc oid, may raise). pg_proc marks each VOLATILE only because what it
/// reads can change during a statement; a call writes nothing and holds nothing once it returns, so it does not act: one
/// made ahead of PostgreSQL's, or more often, changes no row (a VOLATILE call's count is free; a value is never kept
/// across rows). Every evaluation PostgreSQL makes still gets its own call: only the acting readings skip these, never a
/// cache or rerun decision.
/// - The disk sizes (dbsize.c: a relation form opens with AccessShareLock and releases it at relation_close; a missing
///   relation answers NULL). pg_table_size 2997, pg_indexes_size 2998 and pg_total_relation_size 2286 raise on no input.
///   pg_relation_size(regclass, text) 2332 raises 22023 on a fork name that is none. pg_database_size 2324 / 2168 and
///   pg_tablespace_size 2322 / 2323 raise 42501 without CONNECT / CREATE or pg_read_all_stats, the name forms also for a
///   missing name.
/// - pg_lock_status 1371 (lockfuncs.c): copies the lock tables under their LWLocks at its first call, takes no
///   heavyweight lock and checks no privilege; it raises on no input.
/// - pg_stat_have_stats 6230 (pgstatfuncs.c, D4778): looks its key up in the shared stats and the backend's pending
///   entries and creates none; it raises 22023 on a kind name that is none (pgstat_get_kind_from_str).
/// The raise column is each builtin's own answer, kept as data: `call` takes none of them, since a VOLATILE call is never
/// quiet (a reading that asks a call's raise answer reads its no-VOLATILE promise from it too), so each one that runs ahead
/// goes through errorhold's plan-order recheck. pg_relation_size(regclass), 2325, is LANGUAGE sql (the planner inlines it
/// to 2332's call) and stays out, so it fails closed. Each entry is a reviewed rule.
pub const NON_ACTING: [(u32, bool); 10] = [(1371, false), (2168, true), (2286, false), (2322, true), (2323, true), (2324, true), (2332, true), (2997, false), (2998, false), (6230, true)];

/// `Some(may_raise)` when builtin `funcid` is one of `NON_ACTING`.
pub fn non_acting(funcid: u32) -> Option<bool> {
    NON_ACTING.iter().find(|r| r.0 == funcid).map(|r| r.1)
}

/// A LIKE / ILIKE / regex match of a constant `pattern` (its bytes) under collation `collid`: `Some(true)` when the
/// match raises on no input, `Some(false)` when it may, `None` when `funcid` is no pattern match. LIKE's one error is a
/// pattern ending in its escape (MatchText's "LIKE pattern must not end with escape character"), and ILIKE refuses a
/// nondeterministic collation; a regex constant is compiled while planning (its selectivity, regex_fixed_prefix), which
/// raises there on a malformed one. The collation must be the default, C or POSIX (deterministic), and set: under none
/// (0, two implicit collations in conflict) every form raises 42P22 at its first non-null row, LIKE included (like.c
/// GenericMatchText, Generic_Text_IC_like; regexp.c), a raise PostgreSQL meets only on the rows it reaches.
pub fn pattern_quiet(funcid: u32, pattern: &[u8], collid: u32) -> Option<bool> {
    let like = matches!(funcid, 850 | 851 | 1631..=1636 | 1660 | 1661);
    let regex = matches!(funcid, 1238 | 1239 | 1254 | 1256 | 1656..=1659);
    if !(like || regex) {
        return None;
    }
    let tail = pattern.iter().rev().take_while(|&&b| b == b'\\').count();
    Some(matches!(collid, 100 | 950 | 951) && (regex || tail % 2 == 0))
}

/// The text-like targets of an I/O coercion whose input function raises on no text (EEOP_IOCOERCE calls it with typmod
/// -1): text (textin, `cstring_to_text`), varchar and bpchar (varchar_input / bpchar_input skip the length check at
/// typmod -1), name (namein clips at NAMEDATALEN). A typmod's length check is a separate call on top.
pub const fn io_text_target(t: u32) -> bool {
    matches!(t, 25 | 1043 | 1042 | 19)
}

/// Built-in types whose output function raises on no stored value: bool, "char", name, int8, int2, int4, text, oid,
/// float4, float8 (NaN and the infinities print), bpchar, varchar, numeric (NaN and the infinities print), uuid, date
/// (date_out prints an infinite date by name) and time. The timestamps are not listed (timestamp2tm's "timestamp out of
/// range"), nor any array, record, range or user type (their output functions recurse into user code).
pub const fn out_quiet(t: u32) -> bool {
    matches!(t, 16 | 18 | 19 | 20 | 21 | 23 | 25 | 26 | 700 | 701 | 1042 | 1043 | 1700 | 2950 | 1082 | 1083)
}

/// An I/O coercion from `src` to `dst` (CoerceViaIO: `src`'s output function, then `dst`'s input function at typmod -1)
/// raises on no input: a text-like target (`io_text_target`) from a source whose output function raises on no value
/// (`out_quiet`). Every other target's input function parses (int4in, date_in, numeric_in ..) and may raise.
pub const fn io_quiet(src: u32, dst: u32) -> bool {
    io_text_target(dst) && out_quiet(src)
}

/// The argument types XMLELEMENT's and XMLFOREST's values map to XML without raising (PostgreSQL 18.6 xml.c
/// `map_sql_value_to_xml_value`: bool prints true / false, an array maps each element, xml as it is, any other the type's
/// output function, then escape_xml): bool, name, int8, int2, int4, text, oid, float4, float8, bpchar, varchar, numeric,
/// xml, and the one-dimensional or nested arrays of them (an array's elements map by their own type). date, timestamp and
/// timestamptz raise on an infinite value ("XML does not support infinite date values"), bytea's encoding allocates, a
/// user type's output function is user code: none is listed.
pub const fn xml_value_quiet(t: u32) -> bool {
    matches!(t, 16 | 19 | 20 | 21 | 23 | 25 | 26 | 700 | 701 | 1042 | 1043 | 1700 | 142)
        || matches!(t, 1000 | 1003 | 1016 | 1005 | 1007 | 1009 | 1028 | 1021 | 1022 | 1014 | 1015 | 1231 | 143)
}

/// C1: one builtin fact, `(pg_proc oid, proname, where PostgreSQL 18.6 shows it)`. The place reads `<file under the
/// reference src/> <function>: <what it does>`; decompose tests/effects_home.rs pins the oid to its proname in pg_proc.dat
/// and finds the function in the file (for `NOTICE`, an ereport / elog of NOTICE or WARNING in its body).
pub type Fact = (u32, &'static str, &'static str);

/// Whether `funcid` has a row in `table`.
pub fn has(table: &[Fact], funcid: u32) -> bool {
    table.iter().any(|r| r.0 == funcid)
}

/// The non-VOLATILE builtins that may emit a NOTICE or WARNING per call: a call PostgreSQL never makes prints no message,
/// so running one more or one fewer is seen (an act, though the catalog says IMMUTABLE or STABLE). Moved from lowerv4
/// effects.rs (drain's OIDW-30 list, the GUARD design's .notice.tsv), each row now with its source.
pub const NOTICE: [Fact; 56] = [
    (3745, "to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c to_tsvector_byid parses its text)"),
    (3749, "to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsvector -> to_tsvector_byid)"),
    (4209, "to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c jsonb_string_to_tsvector, add_to_tsvector per string)"),
    (4210, "to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c json_string_to_tsvector, add_to_tsvector per string)"),
    (4211, "to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c jsonb_string_to_tsvector_byid)"),
    (4212, "to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c json_string_to_tsvector_byid)"),
    (4213, "jsonb_to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c jsonb_to_tsvector_worker)"),
    (4214, "jsonb_to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c jsonb_to_tsvector_byid)"),
    (4215, "json_to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c json_to_tsvector_worker)"),
    (4216, "json_to_tsvector", "backend/tsearch/ts_parse.c parsetext: NOTICE 'word is too long to be indexed' (to_tsany.c json_to_tsvector_byid)"),
    (3746, "to_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (to_tsany.c to_tsquery_byid)"),
    (3750, "to_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (to_tsquery -> to_tsquery_byid)"),
    (3747, "plainto_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (to_tsany.c plainto_tsquery_byid)"),
    (3751, "plainto_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (plainto_tsquery -> plainto_tsquery_byid)"),
    (5006, "phraseto_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (to_tsany.c phraseto_tsquery_byid)"),
    (5001, "phraseto_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (phraseto_tsquery -> phraseto_tsquery_byid)"),
    (5007, "websearch_to_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (to_tsany.c websearch_to_tsquery_byid)"),
    (5009, "websearch_to_tsquery", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (websearch_to_tsquery -> _byid)"),
    (3612, "tsqueryin", "backend/utils/adt/tsquery.c parse_tsquery: NOTICE 'text-search query doesn't contain lexemes' (tsqueryin parses its text)"),
    (3743, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_byid_opt)"),
    (3744, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_byid -> _byid_opt)"),
    (3754, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_opt -> _byid_opt)"),
    (3755, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline -> _byid_opt)"),
    (4201, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_jsonb_byid_opt, headline_json_value)"),
    (4202, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_jsonb_byid)"),
    (4203, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_jsonb_opt)"),
    (4204, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_jsonb)"),
    (4205, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_json_byid_opt, headline_json_value)"),
    (4206, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_json_byid)"),
    (4207, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_json_opt)"),
    (4208, "ts_headline", "backend/tsearch/ts_parse.c hlparsetext: NOTICE 'word is too long to be indexed' (wparser.c ts_headline_json)"),
    (2893, "xml_in", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (xml_in -> xml_parse)"),
    (2898, "xml_recv", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (xml_recv -> xml_parse)"),
    (2896, "xml", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (texttoxml -> xmlparse -> xml_parse)"),
    (2614, "xmlexists", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (xmlexists -> xpath_internal)"),
    (2931, "xpath", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (xpath -> xpath_internal)"),
    (2932, "xpath", "backend/catalog/system_functions.sql xpath: 'select pg_catalog.xpath($1, $2, ''{}''::pg_catalog.text[])', a call of xpath 2931"),
    (3049, "xpath_exists", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (xpath_exists -> xpath_internal)"),
    (3050, "xpath_exists", "backend/catalog/system_functions.sql xpath_exists: 'select pg_catalog.xpath_exists($1, $2, ''{}''::pg_catalog.text[])', a call of xpath_exists 3049"),
    (3051, "xml_is_well_formed", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (xml_is_well_formed -> wellformed_xml -> xml_parse)"),
    (3052, "xml_is_well_formed_document", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (wellformed_xml -> xml_parse)"),
    (3053, "xml_is_well_formed_content", "backend/utils/adt/xml.c xml_errorHandler: libxml's warnings as WARNING / NOTICE (wellformed_xml -> xml_parse)"),
    (4005, "jsonb_path_exists", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod's .timestamp(n); date.c anytime_typmod_check for .time(n))"),
    (4006, "jsonb_path_query", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (4007, "jsonb_path_query_array", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (4008, "jsonb_path_query_first", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (4009, "jsonb_path_match", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (4010, "jsonb_path_exists_opr", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (4011, "jsonb_path_match_opr", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (1177, "jsonb_path_exists_tz", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (1179, "jsonb_path_query_tz", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (1180, "jsonb_path_query_array_tz", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (2023, "jsonb_path_query_first_tz", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (2030, "jsonb_path_match_tz", "backend/utils/adt/timestamp.c anytimestamp_typmod_check: WARNING 'precision reduced to maximum allowed' (jsonpath_exec.c executeDateTimeMethod)"),
    (1031, "aclitemin", "backend/utils/adt/acl.c aclparse: WARNING 'defaulting grantor to user ID %u' (aclitemin parses its text)"),
    (2556, "pg_tablespace_databases", "backend/utils/adt/misc.c pg_tablespace_databases: WARNING 'global tablespace never has databases' / '%u is not a tablespace OID'"),
];

/// Builtin `funcid` may emit a NOTICE or WARNING per call though it is not VOLATILE (`NOTICE`).
pub fn notice(funcid: u32) -> bool {
    has(&NOTICE, funcid)
}

/// OID-24: builtins that are not VOLATILE but run a query of their own over the relations their arguments name, so a call
/// runs whatever those relations call (a view's nextval, a user function in its target list). Moved from decompose
/// catalog `xml_row_reader`; decompose tests/effects_home.rs derives the same split from pg_proc.dat and the SPI users.
pub const RUNS_OWN_QUERY: [Fact; 6] = [
    (2923, "table_to_xml", "backend/utils/adt/xml.c table_to_xml: table_to_xml_internal runs 'SELECT * FROM <relation>' through SPI (query_to_xml_internal)"),
    (2929, "table_to_xml_and_xmlschema", "backend/utils/adt/xml.c table_to_xml_and_xmlschema: table_to_xml_internal runs the relation's SELECT through SPI"),
    (2933, "schema_to_xml", "backend/utils/adt/xml.c schema_to_xml: schema_to_xml_internal runs table_to_xml_internal per relation of the schema"),
    (2935, "schema_to_xml_and_xmlschema", "backend/utils/adt/xml.c schema_to_xml_and_xmlschema: schema_to_xml_internal per relation"),
    (2936, "database_to_xml", "backend/utils/adt/xml.c database_to_xml: database_to_xml_internal runs schema_to_xml_internal per schema"),
    (2938, "database_to_xml_and_xmlschema", "backend/utils/adt/xml.c database_to_xml_and_xmlschema: database_to_xml_internal per schema"),
];

/// Builtin `funcid` runs a query of its own (`RUNS_OWN_QUERY`).
pub fn runs_own_query(funcid: u32) -> bool {
    has(&RUNS_OWN_QUERY, funcid)
}

/// STABLE builtins that run the input function of a type named at run time and that type's domain CHECKs, either of
/// which may be user code. Moved from decompose catalog `input_validator`.
pub const INPUT_VALIDATORS: [Fact; 2] = [
    (6210, "pg_input_is_valid", "backend/utils/adt/misc.c pg_input_is_valid: pg_input_is_valid_common -> InputFunctionCallSafe of the named type's input function (domain_in runs its CHECKs)"),
    (6211, "pg_input_error_info", "backend/utils/adt/misc.c pg_input_error_info: pg_input_is_valid_common -> InputFunctionCallSafe of the named type's input function (domain_in runs its CHECKs)"),
];

/// Builtin `funcid` runs a type's input function named at run time (`INPUT_VALIDATORS`).
pub fn input_validator(funcid: u32) -> bool {
    has(&INPUT_VALIDATORS, funcid)
}

/// OID-24: builtins that are not VOLATILE and run a statement of their own through SPI over the catalogs alone (the
/// catalog each one queries): no user relation's rows, so no user code runs and neither their volatility nor
/// `RUNS_OWN_QUERY` makes them loud, but the call still runs a statement inside the caller's (D4583). PostgreSQL 18.6's
/// places (each row's line cites). decompose tests/effects_home.rs pins the set to its `CATALOG_ONLY`, which it derives
/// from pg_proc.dat and the SPI users.
pub const CATALOG_QUERY: [Fact; 9] = [
    (1573, "pg_get_ruledef", "backend/utils/adt/ruleutils.c pg_get_ruledef: pg_get_ruledef_worker's SPI_prepare of its pg_rewrite SELECT (ruleutils.c:629) and SPI_execute_plan (641), between SPI_connect (616) and SPI_finish (664)"),
    (2504, "pg_get_ruledef", "backend/utils/adt/ruleutils.c pg_get_ruledef_ext: pg_get_ruledef_worker's SPI_prepare of its pg_rewrite SELECT (ruleutils.c:629) and SPI_execute_plan (641), between SPI_connect (616) and SPI_finish (664)"),
    (1640, "pg_get_viewdef", "backend/utils/adt/ruleutils.c pg_get_viewdef_name: pg_get_viewdef_worker's SPI_prepare of its pg_rewrite SELECT (ruleutils.c:822) and SPI_execute_plan (836), between SPI_connect (808) and SPI_finish (859)"),
    (1641, "pg_get_viewdef", "backend/utils/adt/ruleutils.c pg_get_viewdef: pg_get_viewdef_worker's SPI_prepare of its pg_rewrite SELECT (ruleutils.c:822) and SPI_execute_plan (836), between SPI_connect (808) and SPI_finish (859)"),
    (2505, "pg_get_viewdef", "backend/utils/adt/ruleutils.c pg_get_viewdef_name_ext: pg_get_viewdef_worker's SPI_prepare of its pg_rewrite SELECT (ruleutils.c:822) and SPI_execute_plan (836), between SPI_connect (808) and SPI_finish (859)"),
    (2506, "pg_get_viewdef", "backend/utils/adt/ruleutils.c pg_get_viewdef_ext: pg_get_viewdef_worker's SPI_prepare of its pg_rewrite SELECT (ruleutils.c:822) and SPI_execute_plan (836), between SPI_connect (808) and SPI_finish (859)"),
    (3159, "pg_get_viewdef", "backend/utils/adt/ruleutils.c pg_get_viewdef_wrap: pg_get_viewdef_worker's SPI_prepare of its pg_rewrite SELECT (ruleutils.c:822) and SPI_execute_plan (836), between SPI_connect (808) and SPI_finish (859)"),
    (2934, "schema_to_xmlschema", "backend/utils/adt/xml.c schema_to_xmlschema: schema_to_xmlschema_internal's SPI_connect (xml.c:3284), then schema_get_xml_visible_tables -> query_to_oid_list's SPI_execute of its pg_class SELECT (2791)"),
    (2937, "database_to_xmlschema", "backend/utils/adt/xml.c database_to_xmlschema: database_to_xmlschema_internal's SPI_connect (xml.c:3423), then database_get_xml_visible_schemas / _tables -> query_to_oid_list's SPI_execute over pg_namespace and pg_class (2791)"),
];

/// Builtin `funcid` runs a statement of its own over the catalogs (`CATALOG_QUERY`).
pub fn catalog_query(funcid: u32) -> bool {
    has(&CATALOG_QUERY, funcid)
}

/// The VOLATILE builtins whose act stays inside the call: each returns a fresh value (a sequence's next number, a random
/// draw, a clock reading) and leaves no state another call reads but the state `CALL_LOCAL_STATE` names. A held error
/// may cover a unit that acts only through these (decompose `effects::held_error`): run ahead of PostgreSQL's point, such
/// a call changes no row, only when its value was drawn and how many were drawn, and an extra call is no difference
/// Gravity keeps. `nextval_internal` also holds RowExclusiveLock on the sequence until the transaction ends and may assign
/// an xid when it WAL-logs a fetch; both are what one more call does, so the same holds. The `random` overloads
/// over a range draw from the same PRNG as `drandom` and raise when min > max, which is the held error. Not listed:
/// `uuidv7` (pg_proc 6429 / 6430), whose get_real_time_ns_ascending keeps the previous time in a static, session state
/// that outlives the call.
pub const CALL_LOCAL_ACT: [Fact; 10] = [
    (1574, "nextval", "backend/commands/sequence.c nextval_oid: nextval_internal advances the sequence and returns its next value"),
    (1598, "random", "backend/utils/adt/pseudorandomfuncs.c drandom: a uniform float8 draw from the session's PRNG"),
    (6339, "random", "backend/utils/adt/pseudorandomfuncs.c int4random: an int4 draw in [min, max]; raises when min > max"),
    (6340, "random", "backend/utils/adt/pseudorandomfuncs.c int8random: an int8 draw in [min, max]; raises when min > max"),
    (6341, "random", "backend/utils/adt/pseudorandomfuncs.c numeric_random: a numeric draw in [min, max]; raises when min > max"),
    (6212, "random_normal", "backend/utils/adt/pseudorandomfuncs.c drandom_normal: a normal draw from the session's PRNG"),
    (3432, "gen_random_uuid", "backend/utils/adt/uuid.c gen_random_uuid: a version 4 UUID from pg_strong_random"),
    (6428, "uuidv4", "backend/utils/adt/uuid.c gen_random_uuid: uuidv4's prosrc, the same C function"),
    (2649, "clock_timestamp", "backend/utils/adt/timestamp.c clock_timestamp: GetCurrentTimestamp, the wall clock now"),
    (274, "timeofday", "backend/utils/adt/timestamp.c timeofday: gettimeofday formatted as text"),
];

/// Builtin `funcid` acts only inside its call (`CALL_LOCAL_ACT`).
pub fn call_local_act(funcid: u32) -> bool {
    has(&CALL_LOCAL_ACT, funcid)
}

/// The `CALL_LOCAL_ACT` rows that are independent random draws (their C function draws from the session's PRNG or
/// pg_strong_random): which value a draw returns shows no place among the statement's calls, unlike a sequence's next
/// number or a clock reading (decompose `Found::order_call`).
pub const CALL_LOCAL_DRAW: [u32; 7] = [1598, 6339, 6340, 6341, 6212, 3432, 6428];

/// Builtin `funcid` is a `CALL_LOCAL_DRAW` row.
pub fn call_local_draw(funcid: u32) -> bool {
    CALL_LOCAL_DRAW.contains(&funcid)
}

/// The builtins that read or set the state a `CALL_LOCAL_ACT` call moves or draws from: a sequence's position and the
/// session's last values, the session's PRNG seed. A statement that calls one keeps PostgreSQL's order for each such
/// call: a held one run ahead would change what the reader returns or what the setter's later draws see.
pub const CALL_LOCAL_STATE: [Fact; 7] = [
    (1575, "currval", "backend/commands/sequence.c currval_oid: the session's last value of the sequence"),
    (2559, "lastval", "backend/commands/sequence.c lastval: the session's last nextval value of any sequence"),
    (4032, "pg_sequence_last_value", "backend/commands/sequence.c pg_sequence_last_value: reads the sequence tuple's last_value"),
    (6427, "pg_get_sequence_data", "backend/commands/sequence.c pg_get_sequence_data: reads the sequence tuple"),
    (1576, "setval", "backend/commands/sequence.c setval_oid: do_setval moves the sequence"),
    (1765, "setval", "backend/commands/sequence.c setval3_oid: do_setval moves the sequence, is_called given"),
    (1599, "setseed", "backend/utils/adt/pseudorandomfuncs.c setseed: seeds the session's PRNG"),
];

/// Builtin `funcid` reads or sets the state a `CALL_LOCAL_ACT` call moves (`CALL_LOCAL_STATE`).
pub fn call_local_state(funcid: u32) -> bool {
    has(&CALL_LOCAL_STATE, funcid)
}

/// Reads that raise on no input over a true array (an array a stored column or an array constructor holds): a missing
/// dimension answers NULL. Moved from decompose catalog `array_bound` (read under `Scope::fetch`).
pub const ARRAY_BOUND: [Fact; 2] = [
    (2091, "array_lower", "backend/utils/adt/arrayfuncs.c array_lower: PG_RETURN_NULL for a dimension the array lacks, no ereport"),
    (2092, "array_upper", "backend/utils/adt/arrayfuncs.c array_upper: PG_RETURN_NULL for a dimension the array lacks, no ereport"),
];

/// `array_lower` / `array_upper` (`ARRAY_BOUND`).
pub fn array_bound(funcid: u32) -> bool {
    has(&ARRAY_BOUND, funcid)
}

/// The hash support functions that read their collation and raise 42P22 on a non-NULL value under none (InvalidOid, two
/// implicit collations in conflict): a raise-free hash only under a set collation. `hashname` hashes its bytes under any.
/// Moved from gravity_fn typesem keysem.rs.
pub const HASH_NEEDS_COLLATION: [Fact; 4] = [
    (400, "hashtext", "backend/access/hash/hashfunc.c hashtext: ereport 'could not determine which collation to use for string hashing' when !OidIsValid(collid)"),
    (448, "hashtextextended", "backend/access/hash/hashfunc.c hashtextextended: ereport 'could not determine which collation to use for string hashing' when !OidIsValid(collid)"),
    (1080, "hashbpchar", "backend/utils/adt/varchar.c hashbpchar: ereport 'could not determine which collation to use for string hashing' when !OidIsValid(collid)"),
    (972, "hashbpcharextended", "backend/utils/adt/varchar.c hashbpcharextended: ereport 'could not determine which collation to use for string hashing' when !OidIsValid(collid)"),
];

/// Hash support function `funcid` raises under no collation (`HASH_NEEDS_COLLATION`).
pub fn hash_needs_collation(funcid: u32) -> bool {
    has(&HASH_NEEDS_COLLATION, funcid)
}

/// The collatable-argument builtins that read no collation: neither they nor what they call ask PG_GET_COLLATION, so a call
/// under an indeterminate input collation (InvalidOid: two implicit collations in conflict) runs as under any other and
/// never meets varlena.c check_collation_set's 42P22. The pattern operator classes compare bytes (text_pattern_ops,
/// bpchar_pattern_ops: memcmp, then the length), and `||` copies them.
pub const COLLATION_FREE: [Fact; 11] = [
    (2160, "text_pattern_lt", "backend/utils/adt/varlena.c text_pattern_lt: internal_text_pattern_compare, a memcmp of the two values then their lengths; no PG_GET_COLLATION"),
    (2161, "text_pattern_le", "backend/utils/adt/varlena.c text_pattern_le: internal_text_pattern_compare, a memcmp of the two values then their lengths; no PG_GET_COLLATION"),
    (2163, "text_pattern_ge", "backend/utils/adt/varlena.c text_pattern_ge: internal_text_pattern_compare, a memcmp of the two values then their lengths; no PG_GET_COLLATION"),
    (2164, "text_pattern_gt", "backend/utils/adt/varlena.c text_pattern_gt: internal_text_pattern_compare, a memcmp of the two values then their lengths; no PG_GET_COLLATION"),
    (2166, "bttext_pattern_cmp", "backend/utils/adt/varlena.c bttext_pattern_cmp: internal_text_pattern_compare, a memcmp of the two values then their lengths; no PG_GET_COLLATION"),
    (2174, "bpchar_pattern_lt", "backend/utils/adt/varchar.c bpchar_pattern_lt: internal_bpchar_pattern_compare, a memcmp of the two values to their bcTruelen; no PG_GET_COLLATION"),
    (2175, "bpchar_pattern_le", "backend/utils/adt/varchar.c bpchar_pattern_le: internal_bpchar_pattern_compare, a memcmp of the two values to their bcTruelen; no PG_GET_COLLATION"),
    (2177, "bpchar_pattern_ge", "backend/utils/adt/varchar.c bpchar_pattern_ge: internal_bpchar_pattern_compare, a memcmp of the two values to their bcTruelen; no PG_GET_COLLATION"),
    (2178, "bpchar_pattern_gt", "backend/utils/adt/varchar.c bpchar_pattern_gt: internal_bpchar_pattern_compare, a memcmp of the two values to their bcTruelen; no PG_GET_COLLATION"),
    (2180, "btbpchar_pattern_cmp", "backend/utils/adt/varchar.c btbpchar_pattern_cmp: internal_bpchar_pattern_compare, a memcmp of the two values to their bcTruelen; no PG_GET_COLLATION"),
    (1258, "textcat", "backend/utils/adt/varlena.c textcat: text_catenate copies the two values into one; no PG_GET_COLLATION"),
];

/// Builtin `funcid` reads no collation (`COLLATION_FREE`): an indeterminate input collation raises nothing in it.
pub fn collation_free(funcid: u32) -> bool {
    has(&COLLATION_FREE, funcid)
}

/// The combine functions of the collects whose value lists their inputs in input order: the second state's elements
/// follow the first's, so the result keeps the order the partial states saw (a parallel fold of one keeps it only when
/// the states are combined in the scan's order). Moved from gravity_hosts ops/agg/trans/hosted.rs.
pub const ORDERED_COMBINES: [Fact; 3] = [
    (6293, "array_agg_combine", "backend/utils/adt/array_userfuncs.c array_agg_combine: appends state2's dvalues after state1's"),
    (6296, "array_agg_array_combine", "backend/utils/adt/array_userfuncs.c array_agg_array_combine: appends state2's data after state1's"),
    (6299, "string_agg_combine", "backend/utils/adt/varlena.c string_agg_combine: appends state2's string after state1's"),
];

/// Combine function `funcid` keeps its inputs' order (`ORDERED_COMBINES`).
pub fn ordered_combine(funcid: u32) -> bool {
    has(&ORDERED_COMBINES, funcid)
}

/// The aggregates whose value over a frame of whole peer groups (RANGE / GROUPS) reads no order among the peers: the
/// counts, whose transition adds one per row and reads no value. Moved from lowerv4 effects/visible/merge.rs
/// `window_peers_free`.
pub const PEER_ORDER_FREE: [Fact; 2] = [
    (2147, "count", "backend/utils/adt/int8.c int8inc_any: count(any)'s transition, the count plus one, reads no argument value"),
    (2803, "count", "backend/utils/adt/int8.c int8inc: count(*)'s transition, the count plus one"),
];

/// Window aggregate `winfnoid` over whole peer groups shows no order among them (`PEER_ORDER_FREE`).
pub fn peer_order_free(winfnoid: u32) -> bool {
    has(&PEER_ORDER_FREE, winfnoid)
}

/// Output functions whose text depends on no GUC (a cast to text through one gives the same text in every session):
/// date_out / timestamp_out read DateStyle and TimeZone, interval_out IntervalStyle, byteaout bytea_output, float8out
/// extra_float_digits, so none of them is listed. Moved from gravity_expr nodes/cast.rs (D3043).
pub const GUC_FREE_OUT: [Fact; 12] = [
    (39, "int2out", "backend/utils/adt/int.c int2out: pg_itoa into a fixed buffer, no GUC"),
    (43, "int4out", "backend/utils/adt/int.c int4out: pg_ltoa into a fixed buffer, no GUC"),
    (461, "int8out", "backend/utils/adt/int8.c int8out: pg_lltoa into a fixed buffer, no GUC"),
    (1799, "oidout", "backend/utils/adt/oid.c oidout: snprintf %u, no GUC"),
    (47, "textout", "backend/utils/adt/varlena.c textout: text_to_cstring, the bytes as stored"),
    (1047, "varcharout", "backend/utils/adt/varchar.c varcharout: text_to_cstring, the bytes as stored"),
    (1045, "bpcharout", "backend/utils/adt/varchar.c bpcharout: text_to_cstring, the bytes as stored"),
    (1243, "boolout", "backend/utils/adt/bool.c boolout: 't' or 'f'"),
    (2953, "uuid_out", "backend/utils/adt/uuid.c uuid_out: the 16 bytes as hex with fixed dashes"),
    (1702, "numeric_out", "backend/utils/adt/numeric.c numeric_out: get_str_from_var at the value's own scale, NaN and the infinities by name"),
    (33, "charout", "backend/utils/adt/char.c charout: the byte, or its octal escape"),
    (35, "nameout", "backend/utils/adt/name.c nameout: pstrdup of the name's bytes"),
];

/// Output function `funcid`'s text depends on no GUC (`GUC_FREE_OUT`).
pub fn guc_free_out(funcid: u32) -> bool {
    has(&GUC_FREE_OUT, funcid)
}

/// Which type-cache function a `DISPATCH` function calls per component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// TYPECACHE_CMP_PROC.
    Cmp,
    /// TYPECACHE_EQ_OPR's oprcode.
    Eq,
    /// TYPECACHE_HASH_PROC, or TYPECACHE_HASH_EXTENDED_PROC (true).
    Hash(bool),
}

/// Where a `DISPATCH` function's components are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// The array's element type.
    Elem(Via),
    /// Each attribute of a named composite (`record` itself: not built in).
    Field(Via),
    /// A range's (or, true, a multirange's range's) rngsubopc FUNCTION 1 and rngcanonical; a hash: the subtype's default.
    Range(Via, bool),
    /// I/O, domain and json rows: a type's I/O function or cast, picked by the value's type; not built in.
    Io,
    /// A type or query named by a run-time argument (a domain's CHECK, a view): never built in.
    Named,
}

/// A `DISPATCH` row: the function and where it reaches.
pub type Dispatch = (Fact, Reach);

/// PG 18.6's built-in functions that call a component type's function through the type cache (btrecordimagecmp and the
/// enum family are no DISPATCH functions), and those that run a type's I/O function or a query named at run time. Moved
/// from lowerv4 effects.rs (the GUARD design's .dispatch.tsv / .named.tsv), each row now with its source.
pub const DISPATCH: [Dispatch; 113] = {
    use Reach::{Elem, Field, Io, Named, Range};
    use Via::{Cmp, Eq as E, Hash as H};
    [
        ((382, "btarraycmp", "backend/utils/adt/arrayfuncs.c btarraycmp: the element type's type-cache comparison function"), Elem(Cmp)),
        ((626, "hash_array", "backend/utils/adt/arrayfuncs.c hash_array: the element type's type-cache hash function"), Elem(H(false))),
        ((782, "hash_array_extended", "backend/utils/adt/arrayfuncs.c hash_array_extended: the element type's type-cache extended hash function"), Elem(H(true))),
        ((744, "array_eq", "backend/utils/adt/arrayfuncs.c array_eq: the element type's type-cache equality operator"), Elem(E)),
        ((390, "array_ne", "backend/utils/adt/arrayfuncs.c array_ne: the element type's type-cache equality operator"), Elem(E)),
        ((391, "array_lt", "backend/utils/adt/arrayfuncs.c array_lt: the element type's type-cache comparison function"), Elem(Cmp)),
        ((392, "array_gt", "backend/utils/adt/arrayfuncs.c array_gt: the element type's type-cache comparison function"), Elem(Cmp)),
        ((393, "array_le", "backend/utils/adt/arrayfuncs.c array_le: the element type's type-cache comparison function"), Elem(Cmp)),
        ((396, "array_ge", "backend/utils/adt/arrayfuncs.c array_ge: the element type's type-cache comparison function"), Elem(Cmp)),
        ((2987, "btrecordcmp", "backend/utils/adt/rowtypes.c btrecordcmp: each column type's type-cache comparison function"), Field(Cmp)),
        ((6192, "hash_record", "backend/utils/adt/rowtypes.c hash_record: each column type's type-cache hash function"), Field(H(false))),
        ((6193, "hash_record_extended", "backend/utils/adt/rowtypes.c hash_record_extended: each column type's type-cache extended hash function"), Field(H(true))),
        ((2981, "record_eq", "backend/utils/adt/rowtypes.c record_eq: each column type's type-cache equality operator"), Field(E)),
        ((2982, "record_ne", "backend/utils/adt/rowtypes.c record_ne: each column type's type-cache equality operator"), Field(E)),
        ((2983, "record_lt", "backend/utils/adt/rowtypes.c record_lt: each column type's type-cache comparison function"), Field(Cmp)),
        ((2984, "record_gt", "backend/utils/adt/rowtypes.c record_gt: each column type's type-cache comparison function"), Field(Cmp)),
        ((2985, "record_le", "backend/utils/adt/rowtypes.c record_le: each column type's type-cache comparison function"), Field(Cmp)),
        ((2986, "record_ge", "backend/utils/adt/rowtypes.c record_ge: each column type's type-cache comparison function"), Field(Cmp)),
        ((3870, "range_cmp", "backend/utils/adt/rangetypes.c range_cmp: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((6391, "range_sortsupport", "backend/utils/adt/rangetypes.c range_sortsupport: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((3902, "hash_range", "backend/utils/adt/rangetypes.c hash_range: the subtype's type-cache hash function"), Range(H(false), false)),
        ((3417, "hash_range_extended", "backend/utils/adt/rangetypes.c hash_range_extended: the subtype's type-cache extended hash function"), Range(H(true), false)),
        ((3855, "range_eq", "backend/utils/adt/rangetypes.c range_eq: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((3856, "range_ne", "backend/utils/adt/rangetypes.c range_ne: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((3871, "range_lt", "backend/utils/adt/rangetypes.c range_lt: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((3872, "range_le", "backend/utils/adt/rangetypes.c range_le: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((3873, "range_ge", "backend/utils/adt/rangetypes.c range_ge: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((3874, "range_gt", "backend/utils/adt/rangetypes.c range_gt: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, false)),
        ((4273, "multirange_cmp", "backend/utils/adt/multirangetypes.c multirange_cmp: of each member range: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, true)),
        ((4278, "hash_multirange", "backend/utils/adt/multirangetypes.c hash_multirange: of each member range: the subtype's type-cache hash function"), Range(H(false), true)),
        ((4279, "hash_multirange_extended", "backend/utils/adt/multirangetypes.c hash_multirange_extended: of each member range: the subtype's type-cache extended hash function"), Range(H(true), true)),
        ((4244, "multirange_eq", "backend/utils/adt/multirangetypes.c multirange_eq: of each member range: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, true)),
        ((4245, "multirange_ne", "backend/utils/adt/multirangetypes.c multirange_ne: of each member range: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, true)),
        ((4274, "multirange_lt", "backend/utils/adt/multirangetypes.c multirange_lt: of each member range: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, true)),
        ((4275, "multirange_le", "backend/utils/adt/multirangetypes.c multirange_le: of each member range: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, true)),
        ((4276, "multirange_ge", "backend/utils/adt/multirangetypes.c multirange_ge: of each member range: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, true)),
        ((4277, "multirange_gt", "backend/utils/adt/multirangetypes.c multirange_gt: of each member range: the range's rngsubopc FUNCTION 1 and its canonical function"), Range(Cmp, true)),
        ((750, "array_in", "backend/utils/adt/arrayfuncs.c array_in: the element type's I/O function per element"), Io),
        ((751, "array_out", "backend/utils/adt/arrayfuncs.c array_out: the element type's I/O function per element"), Io),
        ((2400, "array_recv", "backend/utils/adt/arrayfuncs.c array_recv: the element type's I/O function per element"), Io),
        ((2401, "array_send", "backend/utils/adt/arrayfuncs.c array_send: the element type's I/O function per element"), Io),
        ((2296, "anyarray_in", "backend/utils/adt/pseudotypes.c PSEUDOTYPE_DUMMY_INPUT_FUNC(anyarray): raises 'cannot accept a value of type'; listed with the I/O rows, no component call"), Io),
        ((2297, "anyarray_out", "backend/utils/adt/pseudotypes.c anyarray_out: array_out / array_send: the element type's output / send function"), Io),
        ((2502, "anyarray_recv", "backend/utils/adt/pseudotypes.c PSEUDOTYPE_DUMMY_RECEIVE_FUNC(anyarray): raises 'cannot accept a value of type'; listed with the I/O rows, no component call"), Io),
        ((2503, "anyarray_send", "backend/utils/adt/pseudotypes.c anyarray_send: array_out / array_send: the element type's output / send function"), Io),
        ((5088, "anycompatiblearray_in", "backend/utils/adt/pseudotypes.c PSEUDOTYPE_DUMMY_INPUT_FUNC(anycompatiblearray): raises 'cannot accept a value of type'; listed with the I/O rows, no component call"), Io),
        ((5089, "anycompatiblearray_out", "backend/utils/adt/pseudotypes.c anycompatiblearray_out: array_out / array_send: the element type's output / send function"), Io),
        ((5090, "anycompatiblearray_recv", "backend/utils/adt/pseudotypes.c PSEUDOTYPE_DUMMY_RECEIVE_FUNC(anycompatiblearray): raises 'cannot accept a value of type'; listed with the I/O rows, no component call"), Io),
        ((5091, "anycompatiblearray_send", "backend/utils/adt/pseudotypes.c anycompatiblearray_send: array_out / array_send: the element type's output / send function"), Io),
        ((2290, "record_in", "backend/utils/adt/rowtypes.c record_in: each column type's I/O function"), Io),
        ((2291, "record_out", "backend/utils/adt/rowtypes.c record_out: each column type's I/O function"), Io),
        ((2402, "record_recv", "backend/utils/adt/rowtypes.c record_recv: each column type's I/O function"), Io),
        ((2403, "record_send", "backend/utils/adt/rowtypes.c record_send: each column type's I/O function"), Io),
        ((3834, "range_in", "backend/utils/adt/rangetypes.c range_in: the subtype's I/O function (and the canonical function on input)"), Io),
        ((3835, "range_out", "backend/utils/adt/rangetypes.c range_out: the subtype's I/O function (and the canonical function on input)"), Io),
        ((3836, "range_recv", "backend/utils/adt/rangetypes.c range_recv: the subtype's I/O function (and the canonical function on input)"), Io),
        ((3837, "range_send", "backend/utils/adt/rangetypes.c range_send: the subtype's I/O function (and the canonical function on input)"), Io),
        ((3832, "anyrange_in", "backend/utils/adt/pseudotypes.c PSEUDOTYPE_DUMMY_INPUT_FUNC(anyrange): raises 'cannot accept a value of type'; listed with the I/O rows, no component call"), Io),
        ((3833, "anyrange_out", "backend/utils/adt/pseudotypes.c anyrange_out: range_out / multirange_out: the subtype's output function"), Io),
        ((4231, "multirange_in", "backend/utils/adt/multirangetypes.c multirange_in: the subtype's I/O function (and the canonical function on input)"), Io),
        ((4232, "multirange_out", "backend/utils/adt/multirangetypes.c multirange_out: the subtype's I/O function (and the canonical function on input)"), Io),
        ((4233, "multirange_recv", "backend/utils/adt/multirangetypes.c multirange_recv: the subtype's I/O function (and the canonical function on input)"), Io),
        ((4234, "multirange_send", "backend/utils/adt/multirangetypes.c multirange_send: the subtype's I/O function (and the canonical function on input)"), Io),
        ((4229, "anymultirange_in", "backend/utils/adt/pseudotypes.c PSEUDOTYPE_DUMMY_INPUT_FUNC(anymultirange): raises 'cannot accept a value of type'; listed with the I/O rows, no component call"), Io),
        ((4230, "anymultirange_out", "backend/utils/adt/pseudotypes.c anymultirange_out: range_out / multirange_out: the subtype's output function"), Io),
        ((2597, "domain_in", "backend/utils/adt/domains.c domain_in: the base type's input / receive function and the domain's CHECK constraints"), Io),
        ((2598, "domain_recv", "backend/utils/adt/domains.c domain_recv: the base type's input / receive function and the domain's CHECK constraints"), Io),
        ((3176, "to_json", "backend/utils/adt/json.c to_json: each argument type's output function or its cast to json"), Io),
        ((3787, "to_jsonb", "backend/utils/adt/jsonb.c to_jsonb: each argument type's output function or its cast to json"), Io),
        ((3153, "array_to_json", "backend/utils/adt/json.c array_to_json: the element type's I/O function per element"), Io),
        ((3154, "array_to_json", "backend/utils/adt/json.c array_to_json_pretty: the element type's I/O function per element"), Io),
        ((3155, "row_to_json", "backend/utils/adt/json.c row_to_json: each argument type's output function or its cast to json"), Io),
        ((3156, "row_to_json", "backend/utils/adt/json.c row_to_json_pretty: each argument type's output function or its cast to json"), Io),
        ((3173, "json_agg_transfn", "backend/utils/adt/json.c json_agg_transfn: each argument type's output function or its cast to json"), Io),
        ((6275, "json_agg_strict_transfn", "backend/utils/adt/json.c json_agg_strict_transfn: each argument type's output function or its cast to json"), Io),
        ((3265, "jsonb_agg_transfn", "backend/utils/adt/jsonb.c jsonb_agg_transfn: each argument type's output function or its cast to json"), Io),
        ((6283, "jsonb_agg_strict_transfn", "backend/utils/adt/jsonb.c jsonb_agg_strict_transfn: each argument type's output function or its cast to json"), Io),
        ((3180, "json_object_agg_transfn", "backend/utils/adt/json.c json_object_agg_transfn: each argument type's output function or its cast to json"), Io),
        ((6277, "json_object_agg_strict_transfn", "backend/utils/adt/json.c json_object_agg_strict_transfn: each argument type's output function or its cast to json"), Io),
        ((6278, "json_object_agg_unique_transfn", "backend/utils/adt/json.c json_object_agg_unique_transfn: each argument type's output function or its cast to json"), Io),
        ((6279, "json_object_agg_unique_strict_transfn", "backend/utils/adt/json.c json_object_agg_unique_strict_transfn: each argument type's output function or its cast to json"), Io),
        ((3268, "jsonb_object_agg_transfn", "backend/utils/adt/jsonb.c jsonb_object_agg_transfn: each argument type's output function or its cast to json"), Io),
        ((6285, "jsonb_object_agg_strict_transfn", "backend/utils/adt/jsonb.c jsonb_object_agg_strict_transfn: each argument type's output function or its cast to json"), Io),
        ((6286, "jsonb_object_agg_unique_transfn", "backend/utils/adt/jsonb.c jsonb_object_agg_unique_transfn: each argument type's output function or its cast to json"), Io),
        ((6287, "jsonb_object_agg_unique_strict_transfn", "backend/utils/adt/jsonb.c jsonb_object_agg_unique_strict_transfn: each argument type's output function or its cast to json"), Io),
        ((3198, "json_build_array", "backend/utils/adt/json.c json_build_array: each argument type's output function or its cast to json"), Io),
        ((3200, "json_build_object", "backend/utils/adt/json.c json_build_object: each argument type's output function or its cast to json"), Io),
        ((3271, "jsonb_build_array", "backend/utils/adt/jsonb.c jsonb_build_array: each argument type's output function or its cast to json"), Io),
        ((3273, "jsonb_build_object", "backend/utils/adt/jsonb.c jsonb_build_object: each argument type's output function or its cast to json"), Io),
        ((3960, "json_populate_record", "backend/utils/adt/jsonfuncs.c json_populate_record: each target column type's input function"), Io),
        ((3961, "json_populate_recordset", "backend/utils/adt/jsonfuncs.c json_populate_recordset: each target column type's input function"), Io),
        ((3204, "json_to_record", "backend/utils/adt/jsonfuncs.c json_to_record: each target column type's input function"), Io),
        ((3205, "json_to_recordset", "backend/utils/adt/jsonfuncs.c json_to_recordset: each target column type's input function"), Io),
        ((3209, "jsonb_populate_record", "backend/utils/adt/jsonfuncs.c jsonb_populate_record: each target column type's input function"), Io),
        ((6338, "jsonb_populate_record_valid", "backend/utils/adt/jsonfuncs.c jsonb_populate_record_valid: each target column type's input function"), Io),
        ((3475, "jsonb_populate_recordset", "backend/utils/adt/jsonfuncs.c jsonb_populate_recordset: each target column type's input function"), Io),
        ((3490, "jsonb_to_record", "backend/utils/adt/jsonfuncs.c jsonb_to_record: each target column type's input function"), Io),
        ((3491, "jsonb_to_recordset", "backend/utils/adt/jsonfuncs.c jsonb_to_recordset: each target column type's input function"), Io),
        ((6210, "pg_input_is_valid", "backend/utils/adt/misc.c pg_input_is_valid: the input function of a type named at run time (a domain's CHECK too)"), Named),
        ((6211, "pg_input_error_info", "backend/utils/adt/misc.c pg_input_error_info: the input function of a type named at run time (a domain's CHECK too)"), Named),
        ((2923, "table_to_xml", "backend/utils/adt/xml.c table_to_xml: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2924, "query_to_xml", "backend/utils/adt/xml.c query_to_xml: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2925, "cursor_to_xml", "backend/utils/adt/xml.c cursor_to_xml: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2927, "query_to_xmlschema", "backend/utils/adt/xml.c query_to_xmlschema: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2929, "table_to_xml_and_xmlschema", "backend/utils/adt/xml.c table_to_xml_and_xmlschema: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2930, "query_to_xml_and_xmlschema", "backend/utils/adt/xml.c query_to_xml_and_xmlschema: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2933, "schema_to_xml", "backend/utils/adt/xml.c schema_to_xml: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2935, "schema_to_xml_and_xmlschema", "backend/utils/adt/xml.c schema_to_xml_and_xmlschema: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2936, "database_to_xml", "backend/utils/adt/xml.c database_to_xml: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((2938, "database_to_xml_and_xmlschema", "backend/utils/adt/xml.c database_to_xml_and_xmlschema: runs a query over the relation, cursor, schema or database named at run time (SPI)"), Named),
        ((3685, "ts_rewrite", "backend/utils/adt/tsquery_rewrite.c tsquery_rewrite_query: runs the query its text argument names (SPI)"), Named),
        ((3689, "ts_stat", "backend/utils/adt/tsvector_op.c ts_stat1: runs the query its text argument names (SPI)"), Named),
        ((3690, "ts_stat", "backend/utils/adt/tsvector_op.c ts_stat2: runs the query its text argument names (SPI)"), Named),
    ]
};

/// Where `DISPATCH` function `funcid` reaches; None: no DISPATCH function.
pub fn dispatch(funcid: u32) -> Option<Reach> {
    DISPATCH.iter().find(|d| d.0.0 == funcid).map(|d| d.1)
}

/// The procs every named composite compares and hashes by (its column types' own procs, called per live column; rowtypes.c),
/// in the order equality, comparison, hash. Moved from decompose effects/mod.rs `of_procs_at` (AGC-20).
pub const RECORD_KEY_PROCS: [Fact; 3] = [
    (2981, "record_eq", "backend/utils/adt/rowtypes.c record_eq: each live column's type-cache equality operator"),
    (2987, "btrecordcmp", "backend/utils/adt/rowtypes.c btrecordcmp: record_cmp, each live column's type-cache comparison function"),
    (6192, "hash_record", "backend/utils/adt/rowtypes.c hash_record: each live column's type-cache hash function"),
];

/// The procs every enum compares and hashes by (pg_enum's sort order; no component call), in the order equality,
/// comparison, hash. Moved from decompose effects/mod.rs `of_procs_at` (AGC-20).
pub const ENUM_KEY_PROCS: [Fact; 3] = [
    (3508, "enum_eq", "backend/utils/adt/enum.c enum_eq: compares the two values' oids"),
    (3514, "enum_cmp", "backend/utils/adt/enum.c enum_cmp: enum_cmp_internal, by pg_enum's enumsortorder"),
    (3515, "hashenum", "backend/access/hash/hashfunc.c hashenum: hashes the value's oid"),
];

/// Every C1 fact table, by name: decompose tests/effects_home.rs pins each row.
pub const TABLES: [(&str, &[Fact]); 14] = [
    ("NOTICE", &NOTICE),
    ("RUNS_OWN_QUERY", &RUNS_OWN_QUERY),
    ("CATALOG_QUERY", &CATALOG_QUERY),
    ("INPUT_VALIDATORS", &INPUT_VALIDATORS),
    ("CALL_LOCAL_ACT", &CALL_LOCAL_ACT),
    ("CALL_LOCAL_STATE", &CALL_LOCAL_STATE),
    ("ARRAY_BOUND", &ARRAY_BOUND),
    ("HASH_NEEDS_COLLATION", &HASH_NEEDS_COLLATION),
    ("COLLATION_FREE", &COLLATION_FREE),
    ("ORDERED_COMBINES", &ORDERED_COMBINES),
    ("PEER_ORDER_FREE", &PEER_ORDER_FREE),
    ("GUC_FREE_OUT", &GUC_FREE_OUT),
    ("RECORD_KEY_PROCS", &RECORD_KEY_PROCS),
    ("ENUM_KEY_PROCS", &ENUM_KEY_PROCS),
];

#[cfg(test)]
mod tests {
    #[test]
    fn array_eq_quiet_takes_one_listed_array_type() {
        use super::array_eq_quiet as q;
        assert!(q(744, 1009, 1009) && q(390, 1007, 1007), "text[] = text[], int4[] <> int4[]");
        assert!(!q(744, 1009, 1007), "two array types");
        assert!(!q(744, 2277, 2277), "anyarray");
        assert!(!q(744, 1021, 1021), "float4[]: not listed");
        assert!(!q(382, 1009, 1009), "btarraycmp is another proc");
    }

    #[test]
    fn bit_index_reads_the_length_and_the_index() {
        use super::bit_index as q;
        assert_eq!(q(3032, 8, Some(0)), Some(true));
        assert_eq!(q(3032, 8, Some(7)), Some(true));
        assert_eq!(q(3032, 8, Some(8)), Some(false), "past the last bit");
        assert_eq!(q(3032, 8, Some(-1)), Some(false));
        assert_eq!(q(3032, -1, Some(0)), Some(false), "a length no typmod states");
        assert_eq!(q(3032, 8, None), Some(false), "a NULL or non-constant index");
        assert_eq!(q(723, 8, Some(0)), None, "bytea's get_bit reads no typmod");
    }

    #[test]
    fn the_epoch_ns_multiplier_is_a_finite_numeric_of_at_most_1e9() {
        use super::epoch_ns_k_quiet as q;
        // numeric.c images (little-endian): the short form's header 0x8000 | dscale << 7 | weight, then base-10000 digits
        assert!(q(&[0x02, 0x80, 0x0a, 0x00]), "1e9 (weight 2, digits [10])");
        assert!(q(&[0x02, 0xa0, 0x0a, 0x00]), "-1e9 (the short form's sign 0x2000)");
        assert!(!q(&[0x82, 0x80, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x13]), "1000000000.5");
        assert!(!q(&[0x03, 0x80, 0x01, 0x00]), "1e12 (weight 3)");
        assert!(q(&[0x02, 0x80, 0x09, 0x00, 0x0f, 0x27, 0x0f, 0x27]), "999999999 (digits [9, 9999, 9999])");
        assert!(q(&[0x00, 0x80]), "0");
        assert!(q(&[0xff, 0x80, 0x88, 0x13]), "0.5 (weight -1: the sign 0x0040 over 0x3F, dscale 1)");
        assert!(q(&[0x00, 0x00, 0x02, 0x00, 0x0a, 0x00]), "1e9 in the long form (weight int16 2)");
        assert!(!q(&[0x00, 0xc0]) && !q(&[0x00, 0xd0]) && !q(&[0x00, 0xf0]), "NaN, infinity, -infinity");
        assert!(!q(&[]) && !q(&[0x02, 0x80, 0x0a]), "a short or odd image");
        assert!(super::epoch_clock_value(3) && super::epoch_clock_value(4) && !super::epoch_clock_value(7), "CURRENT_TIMESTAMP[(n)], not LOCALTIMESTAMP");
    }

    #[test]
    fn enum_eq_and_ne_are_quiet_calls() {
        assert!(super::call(3508) && super::call(3509));
    }

    /// D3816: the pinned non-acting list is exactly these oids with these raise answers; an addition is a reviewed change.
    #[test]
    fn non_acting_is_exactly_the_pinned_oids() {
        let pinned = [(1371, false), (2168, true), (2286, false), (2322, true), (2323, true), (2324, true), (2332, true), (2997, false), (2998, false), (6230, true)];
        assert_eq!(super::NON_ACTING, pinned);
        assert_eq!(super::non_acting(2325), None, "pg_relation_size(regclass) is LANGUAGE sql: out");
        assert_eq!(super::non_acting(1), None);
        for (oid, raises) in pinned {
            assert!(!super::call(oid), "oid {oid} (may raise {raises}): VOLATILE, so never quiet");
        }
    }

    #[test]
    fn row_number_rank_and_dense_rank_are_quiet_window_calls() {
        assert!(super::window_quiet(3100, 0) && super::window_quiet(3101, 0) && super::window_quiet(3102, 0));
        assert!(!super::window_quiet(3105, 1) && !super::window_quiet(3114, 2)); // ntile and nth_value raise on a bad argument
        assert!(!super::window_quiet(3100, 1));
    }

    use super::*;

    #[test]
    fn integer_arithmetic_is_quiet_when_its_operand_ranges_keep_the_result_in_its_type() {
        let (add4, mul8, add2, neg4) = (int_arith(177).unwrap(), int_arith(1276).unwrap(), int_arith(176).unwrap(), int_arith(212).unwrap());
        assert!(int_arith_quiet(add4.0, add4.1, I2, I2), "s::int4 + t::int4 over int2 columns");
        assert!(!int_arith_quiet(add4.0, add4.1, I4, I4), "int4col + int4col can leave int4");
        assert!(int_arith_quiet(mul8.0, mul8.1, I4, I4), "a::int8 * b over int4 columns: |a * b| <= 2^62");
        assert!(!int_arith_quiet(mul8.0, mul8.1, I8, I4), "int8col * int4col can leave int8");
        assert!(!int_arith_quiet(add2.0, add2.1, I2, I2), "int2 + int2 is int2: it can leave it");
        assert!(!int_arith_quiet(neg4.0, neg4.1, I4, I4), "-int4col: -INT_MIN leaves int4");
        assert!(int_arith_quiet(neg4.0, neg4.1, I2, I2), "-(s::int4)");
        assert_eq!(int_arith_range(IntOp::Sub, (0, 10), (-3, 4)), (-4, 13));
        assert_eq!(int_arith_range(IntOp::Mul, (-2, 3), (-5, 7)), (-15, 21));
        assert!(int_widen(481) && int_widen(313) && int_widen(754) && !int_widen(480), "int4(int8) narrows: it can raise");
        assert_eq!(int_arith(1726), None, "numeric_mul is no integer operation");
    }

    #[test]
    fn numeric_arithmetic_names_how_it_bounds_its_digits_division_is_not_listed() {
        assert_eq!(num_arith(1724), Some((NumOp::AddSub, 2)));
        assert_eq!(num_arith(1726), Some((NumOp::Mul, 2)));
        assert_eq!(num_arith(1771), Some((NumOp::Neg, 1)));
        assert_eq!(num_arith(1740), Some((NumOp::Cast, 1)), "numeric(int4)");
        assert_eq!(num_arith(1727), None, "numeric_div: its quotient's digits are not its operands'");
        assert_eq!((num_arith(1712), num_arith(1711), num_arith(2167)), (Some((NumOp::Integral, 1)), Some((NumOp::Integral, 1)), Some((NumOp::Integral, 1))), "floor / ceil / ceiling(numeric)");
        assert_eq!(num_arith(2309), None, "floor(float8) is no numeric operation");
        assert!(!call(1712) && !call(1711), "numeric floor / ceil raise past numeric's weight: only under a digits bound");
        assert_eq!((int_digits(21), int_digits(23), int_digits(20), int_digits(1700)), (Some(5), Some(10), Some(19), None));
    }

    #[test]
    fn a_like_pattern_is_quiet_unless_it_ends_in_its_escape() {
        assert_eq!(pattern_quiet(850, b"%garden%", 100), Some(true));
        assert_eq!(pattern_quiet(850, b"\\", 100), Some(false));
        assert_eq!(pattern_quiet(1633, b"a\\\\", 100), Some(true));
        assert_eq!(pattern_quiet(851, b"r%\\", 0), Some(false));
        assert_eq!(pattern_quiet(850, b"%x%", 0), Some(false), "LIKE under an unset collation raises 42P22");
        assert_eq!(pattern_quiet(1633, b"%x%", 0), Some(false), "ILIKE under an unset collation raises 42P22");
        assert_eq!(pattern_quiet(1254, b"^r2", 0), Some(false), "a regex under an unset collation raises 42P22");
        assert_eq!(pattern_quiet(1254, b"^r2", 100), Some(true));
        assert_eq!(pattern_quiet(850, b"%x%", 12345), Some(false));
        assert_eq!(pattern_quiet(67, b"x", 100), None);
        assert!(numeric_cmp(1721) && !numeric_cmp(2338));
        assert!(call(2854) && call(2855) && !call(2853), "the temp-schema functions (DC-14)");
        assert!(int_mod(156) && int_mod(945) && !int_mod(154));
    }

    #[test]
    fn an_integer_modulus_is_quiet_by_a_nonzero_constant_only() {
        for f in [155, 156, 945, 940, 941, 947] {   // int2mod, int4mod, int8mod; mod(int2 / int4 / int8, same type)
            assert!(!int_mod_quiet(f, Some(0)), "{f}: a zero divisor raises 22012");
            assert!(!int_mod_quiet(f, None), "{f}: a NULL (or non-constant) divisor is not taken");
            // -1: INT16_MIN / INT32_MIN / INT64_MIN % -1 is 0 in PostgreSQL, never an overflow
            assert!(int_mod_quiet(f, Some(-1)) && int_mod_quiet(f, Some(1)) && int_mod_quiet(f, Some(7)));
        }
        assert!(!int_mod_quiet(154, Some(7)) && !int_mod_quiet(1728, Some(7)), "int4div, numeric_mod: not the rule's");
    }

    #[test]
    fn an_integer_division_is_quiet_by_a_constant_other_than_zero_and_minus_one() {
        for f in [153, 154, 466, 172, 173, 1281, 1277, 948, 840] {
            assert!(!int_div_quiet(f, Some(0)), "{f}: a zero divisor raises 22012");
            assert!(!int_div_quiet(f, Some(-1)), "{f}: -1 raises 22003 over the dividend type's minimum (declined for all)");
            assert!(!int_div_quiet(f, None), "{f}: a NULL (or non-constant) divisor is not taken");
            assert!(int_div_quiet(f, Some(1)) && int_div_quiet(f, Some(10)) && int_div_quiet(f, Some(-2)) && int_div_quiet(f, Some(i64::MIN)));
        }
        assert!(!int_div_quiet(156, Some(7)) && !int_div_quiet(1727, Some(7)) && !int_div_quiet(217, Some(7)), "int4mod, numeric_div, float8div: not the rule's");
        assert_eq!(divisor_quiet(154, Some(10)), Some(true));
        assert_eq!(divisor_quiet(154, Some(-1)), Some(false));
        assert_eq!(divisor_quiet(156, Some(-1)), Some(true), "int4mod by -1 is 0");
        assert_eq!(divisor_quiet(156, Some(0)), Some(false));
        assert_eq!(divisor_quiet(941, Some(7)), Some(true), "mod(int4, int4) is int4mod");
        assert_eq!(divisor_quiet(947, Some(0)), Some(false), "mod(int8, int8) by 0 raises as int8mod does");
        assert_eq!(divisor_quiet(1728, Some(7)), None, "numeric_mod: not the rule's");
        assert_eq!(divisor_quiet(177, Some(7)), None, "int4pl: no divisor");
    }

    #[test]
    fn an_integer_word_reads_its_low_bytes_sign_extended() {
        assert_eq!(int_word(usize::MAX, 4), Some(-1), "Int32GetDatum(-1) sign-extends");
        assert_eq!(int_word(0xFFFF_FFFF, 4), Some(-1), "a zero-extended word reads the same");
        assert_eq!(int_word(0xFFFF, 2), Some(-1));
        assert_eq!(int_word(0x1_0000, 2), Some(0), "an int2 reads its low 16 bits");
        assert_eq!(int_word(10, 8), Some(10));
        assert_eq!(int_word(usize::MAX, 8), Some(-1));
        assert_eq!(int_word(10, 1), None);
    }

    #[test]
    fn a_call_is_quiet_by_its_own_rule_only() {
        assert!(call(1067) && call(1069) && call(2079) && call(3478) && call(1718) && call(2331) && call(1769));
        assert!(call(1191) && call(1192), "generate_subscripts: no subscript past the dimension, no error");
        assert!(!call(1066) && !call(1068), "generate_series with a step: a zero step raises");
        assert!(!call(156) && !call(154), "int4mod needs its divisor; int4div raises");
    }

    #[test]
    fn the_start_times_are_quiet_the_clock_is_not_listed() {
        for f in [1299, 2647, 2648] {   // now(), transaction_timestamp(), statement_timestamp()
            assert!(start_time(f) && call(f), "{f}");
        }
        assert!(!call(2649) && !call(274), "clock_timestamp(), timeofday(): not the rule's");
        assert!((0..=8).all(start_time_value), "CURRENT_DATE .. LOCALTIMESTAMP(n)");
        assert!(!(9..=14).any(start_time_value), "CURRENT_ROLE .. CURRENT_SCHEMA: not the rule's");
    }

    #[test]
    fn integer_bitwise_operators_are_quiet_shifts_are_not_listed() {
        for f in [1892, 1893, 1894, 1895, 1898, 1899, 1900, 1901, 1904, 1905, 1906, 1907] {
            assert!(int_bitwise(f) && call(f), "{f}");
        }
        assert!(!int_bitwise(1902) && !int_bitwise(1903) && !int_bitwise(1896), "int4shl, int4shr, int2shl: not the rule's");
    }

    #[test]
    fn only_the_four_privilege_functions_read_their_text() {
        for f in [2710, 1927, 3029, 3143, 3023] {
            assert!(privilege_fn(f), "{f}");
        }
        assert_eq!((privilege_args(2710), privilege_args(3023), privilege_args(3022)), (Some(2), Some(3), None), "3022: a column name");
        // textlike, texteq, int4pl, has_table_privilege's name form (text, text)
        for f in [850, 67, 177, 1922] {
            assert!(!privilege_fn(f), "{f}");
        }
    }

    #[test]
    fn a_privilege_text_is_quiet_when_acl_c_accepts_every_chunk() {
        assert_eq!(privilege_quiet(2710, b"USAGE"), Some(true));
        assert_eq!(privilege_quiet(2710, b"usage"), Some(true), "pg_strcasecmp");
        assert_eq!(privilege_quiet(2710, b" member , SET WITH ADMIN OPTION "), Some(true));
        assert_eq!(privilege_quiet(1927, b"INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER"), Some(true));
        assert_eq!(privilege_quiet(1927, b"maintain with grant option"), Some(true));
        assert_eq!(privilege_quiet(3029, b"INSERT, UPDATE, REFERENCES"), Some(true));
        assert_eq!(privilege_quiet(3143, b"USAGE"), Some(true));
        assert_eq!(privilege_quiet(3143, b"USAGE WITH ADMIN OPTION"), Some(false), "admin option is a role's only");
        assert_eq!(privilege_quiet(2261, b"EXECUTE"), Some(true), "has_function_privilege(oid, text)");
        assert_eq!(privilege_quiet(2261, b"execute with grant option"), Some(true));
        assert_eq!(privilege_quiet(2261, b"USAGE"), Some(false), "not a function privilege");
        assert_eq!(privilege_quiet(2260, b"EXECUTE"), None, "has_function_privilege(text, text): a literal name can raise");
        assert_eq!(privilege_quiet(3029, b"DELETE"), Some(false), "not a column privilege");
        assert_eq!(privilege_quiet(1927, b"USAGE"), Some(false));
        assert_eq!(privilege_quiet(1927, b""), Some(false), "an empty chunk raises");
        assert_eq!(privilege_quiet(1927, b"SELECT,"), Some(false), "a trailing comma is an empty chunk");
        assert_eq!(privilege_quiet(1927, b"SELECT  WITH GRANT OPTION"), Some(false), "no whitespace freedom inside a name");
        assert_eq!(privilege_quiet(1927, b"SEL\xc9CT"), Some(false));
        assert_eq!(privilege_quiet(2709, b"USAGE"), None, "pg_has_role(name, text): a literal name can raise");
        assert_eq!(privilege_quiet(1922, b"SELECT"), None, "has_table_privilege(name, text, text): not the rule's");
        assert_eq!(privilege_quiet(3023, b"SELECT, INSERT, UPDATE, REFERENCES"), Some(true), "information_schema.columns");
        assert_eq!(privilege_quiet(3023, b"update with grant option"), Some(true));
        assert_eq!(privilege_quiet(3023, b"DELETE"), Some(false), "not a column privilege");
    }

    #[test]
    fn nameconcatoid_is_quiet() {
        assert!(call(NAMECONCATOID));
    }

    #[test]
    fn expandarray_reads_quiet_only_with_its_pg_body() {
        let (nsp, name, src) = (EXPANDARRAY.0.as_bytes(), EXPANDARRAY.1.as_bytes(), EXPANDARRAY.2.as_bytes());
        assert!(sql_body(nsp, name, SQL_LANGUAGE, src));
        assert!(!sql_body(nsp, name, SQL_LANGUAGE, b"SELECT $1[1], 1 / 0"), "a replaced body");
        assert!(!sql_body(b"public", name, SQL_LANGUAGE, src) && !sql_body(nsp, name, 13, src), "another schema or language");
    }

    #[test]
    fn jsonb_exists_is_quiet() {
        assert!(call(JSONB_EXISTS) && !call(4048) && !call(4049), "jsonb ?| / ?& are not the rule's");
    }

    #[test]
    fn jsonb_containment_is_quiet_path_reads_are_not_listed() {
        assert!(call(4046) && call(4050), "jsonb @> / <@");
        assert!(!call(3940) && !call(4011), "jsonb_extract_path_text, jsonb_path_match_opr: not the rule's");
    }

    #[test]
    fn text_length_is_quiet_its_neighbours_are_not_listed() {
        assert!(TEXT_LENGTH.iter().all(|&f| call(f)), "length / char_length / character_length / textlen of text");
        assert!(!call(1318) && !call(2010), "length(bpchar) bpcharlen, length(bytea) byteaoctetlen: not the rule's");
    }

    #[test]
    fn octet_length_of_text_and_bpchar_is_quiet() {
        assert!(OCTET_LENGTH.iter().all(|&f| call(f)), "octet_length of text / bpchar");
        assert!(!call(720) && !call(1682), "octet_length(bytea), octet_length(bit): not the rule's");
    }

    #[test]
    fn jsonb_field_and_element_accessors_are_quiet_path_extraction_is_not_listed() {
        for f in [3478, 3214, 3215, 3216] {   // -> text, ->> text, -> int4, ->> int4
            assert!(jsonb_field(f), "{f}");
        }
        assert!(!jsonb_field(3217) && !jsonb_field(3940), "jsonb_extract_path(_text): not the rule's");
    }

    #[test]
    fn a_substr_is_quiet_for_a_constant_length_not_below_zero() {
        for f in [877, 936] {
            assert!(substr_quiet(f, Some(1)) && substr_quiet(f, Some(0)), "{f}");
            assert!(!substr_quiet(f, Some(-1)), "{f}: a negative length raises");
            assert!(!substr_quiet(f, None), "{f}: a NULL or non-constant length is the other class");
        }
        assert!(!substr_quiet(883, Some(1)), "substr(text, int4): not the rule's");
    }

    #[test]
    fn left_and_right_are_quiet_for_any_constant_but_int32_min() {
        for f in [3060, 3061] {
            assert!(left_right_quiet(f, Some(4)) && left_right_quiet(f, Some(-4)) && left_right_quiet(f, Some(0)), "{f}");
            assert!(!left_right_quiet(f, Some(i32::MIN as i64)), "{f}: INT32_MIN's negation overflows");
            assert!(!left_right_quiet(f, None), "{f}");
        }
        assert!(!left_right_quiet(877, Some(4)), "substr: not the rule's");
    }

    #[test]
    fn case_maps_are_quiet_only_under_the_default_c_or_posix_collation() {
        for f in [870, 871, 872] {
            for c in [100, 950, 951] {
                assert!(case_map_quiet(f, c), "{f} under {c}");
            }
            assert!(!case_map_quiet(f, 0), "{f}: an unset collation raises 42P22");
            assert!(!case_map_quiet(f, 12345), "{f}: another collation (its own locale lookup and provider)");
        }
        assert!(!case_map_quiet(3848, 100), "lower(anyrange): not the rule's");
        assert!(!case_map_quiet(4235, 100), "lower(anymultirange): not the rule's");
    }

    #[test]
    fn text_extrema_are_quiet_only_under_a_bytewise_or_default_collation() {
        for f in [2129, 2145] {
            for c in [100, 950, 951] {
                assert!(text_extremum_quiet(f, c), "{f} under {c}");
            }
            assert!(!text_extremum_quiet(f, 12345), "{f}: another collation");
            assert!(!text_extremum_quiet(f, 0), "{f}: an indeterminate collation raises 42P22 at the first comparison");
        }
        assert!(!text_extremum_quiet(2130, 100), "max(numeric): not the rule's");
    }

    #[test]
    fn extrema_are_quiet_over_the_comparison_quiet_types_only() {
        // max / min of int8 int4 int2 oid float4 float8 date time timetz money timestamp timestamptz interval numeric tid
        // pg_lsn xid8 (PostgreSQL 18.6 pg_proc oids).
        for f in [2115, 2116, 2117, 2118, 2119, 2120, 2122, 2123, 2124, 2125, 2126, 2127, 2128, 2130, 2797, 4189, 5099] {
            assert!(extremum_quiet(f), "max {f}");
        }
        for f in [2131, 2132, 2133, 2134, 2135, 2136, 2138, 2139, 2140, 2141, 2142, 2143, 2144, 2146, 2798, 4190, 5100] {
            assert!(extremum_quiet(f), "min {f}");
        }
        // text (a collation: text_extremum_quiet), anyarray, record, bpchar, inet, anyenum, bytea; sum(int4).
        for f in [2129, 2145, 2050, 2051, 6373, 6374, 2244, 2245, 3564, 3565, 3526, 3527, 6395, 6396, 2108] {
            assert!(!extremum_quiet(f), "{f}");
        }
        for t in [20, 21, 23, 26, 27, 700, 701, 790, 1082, 1083, 1114, 1184, 1186, 1266, 1700, 3220, 5069] {
            assert!(compare_quiet(t), "type {t}");
        }
        assert!(distinct_count_quiet(2147, 1700) && distinct_count_quiet(2147, 1186), "count(DISTINCT numeric / interval)");
        assert!(!distinct_count_quiet(2147, 25) && !distinct_count_quiet(2803, 23), "count(DISTINCT text); count(*)");
        for t in [25, 1042, 1043, 17, 869, 2277, 2249] {
            assert!(!compare_quiet(t), "type {t}: collatable, an array, a record, inet or bytea: not listed");
        }
    }

    #[test]
    fn agg_quiet_is_the_counts_and_the_comparison_quiet_extrema() {
        assert!(agg_quiet(2803, 0) && agg_quiet(2147, 0), "count(*), count(any)");
        for f in 2115..=5100u32 {
            assert_eq!(agg_quiet(f, 0), matches!(f, 2803 | 2147) || extremum_quiet(f), "{f}: the counts and every extremum_quiet aggregate");
        }
        assert!(agg_quiet(2124, 0) && agg_quiet(2140, 0) && agg_quiet(2128, 0) && agg_quiet(2144, 0), "timetz / interval: NEW-44");
        assert!(agg_quiet(2797, 0) && agg_quiet(4189, 0) && agg_quiet(5100, 0), "tid, pg_lsn, xid8");
        assert!(agg_quiet(2129, 100) && !agg_quiet(2129, 12345), "max(text): text_extremum_quiet's collations");
        for f in [2108, 2101, 2114, 3526, 2244, 3564, 2050, 2100] {
            assert!(!agg_quiet(f, 100), "{f}: sum / avg / an enum, bpchar, inet or array extremum");
        }
    }

    #[test]
    fn io_coercion_is_quiet_only_to_a_text_like_type_from_a_listed_source() {
        for dst in [25, 1043, 1042, 19] {
            for src in [23, 20, 21, 1700, 701, 16, 25, 1082, 2950] {
                assert!(io_quiet(src, dst), "{src} -> {dst}");
            }
            assert!(!io_quiet(1114, dst) && !io_quiet(1184, dst), "a timestamp's output may raise out of range");
            assert!(!io_quiet(1007, dst) && !io_quiet(2249, dst) && !io_quiet(16384, dst), "an array, a record, a user type");
        }
        assert!(!io_quiet(25, 23) && !io_quiet(25, 1082) && !io_quiet(25, 1700), "int4in, date_in, numeric_in parse");
    }

    #[test]
    fn xml_values_are_quiet_for_the_listed_types_and_their_arrays() {
        for t in [16, 23, 20, 25, 1043, 1700, 701, 142, 1007, 1009, 143] {
            assert!(xml_value_quiet(t), "{t}");
        }
        for t in [1082, 1114, 1184, 17, 2249, 16384, 1182] {
            assert!(!xml_value_quiet(t), "{t}: date / timestamps raise on infinity, bytea, a record, a user type, date[]");
        }
    }

    #[test]
    fn textcat_is_quiet_its_one_raise_is_a_resource_limit() {
        assert!(call(TEXTCAT), "text || text");
        assert!(!call(2003) && !call(2004) && !call(2011), "textanycat, anytextcat (an output function), byteacat: not the rule's");
    }

    #[test]
    fn char_text_conversions_are_quiet() {
        assert!(call(946) && call(944), "text(\"char\"), \"char\"(text)");
        assert!(!call(1714) && !call(1717), "convert_from, convert_to: not the rule's");
    }

    #[test]
    fn datetime_units_decode_and_class_as_postgres() {
        // D3041: datebsearch's 10-byte prefix, downcasing, deltatktbl before datetktbl, `@` / `ago` / a reserved word
        assert_eq!(dt_unit(b"MINUTE", true), Some(DtUnit::Min));
        assert_eq!(dt_unit(b"microseconds", true), Some(DtUnit::Usec));
        assert_eq!(dt_unit(b"millisecondsXYZ", false), Some(DtUnit::Msec));
        assert_eq!(dt_unit(b"m", true), Some(DtUnit::Min));
        assert_eq!(dt_unit(b"mm", true), Some(DtUnit::Min));
        assert_eq!(dt_unit(b"mm", false), None);
        assert_eq!(dt_unit(b"dow", false), None);
        assert_eq!(dt_unit(b"ago", true), Some(DtUnit::Other));
        assert_eq!(dt_unit(b"minutex", true), None);
        assert_eq!(dt_unit(b"now", true), None);
        assert_eq!(datetime_unit_quiet(6202, b"minute"), Some(true));
        assert_eq!(datetime_unit_quiet(6202, b"epoch"), Some(true));
        assert_eq!(datetime_unit_quiet(6202, b"timezone"), Some(false), "not supported for a finite timestamp");
        assert_eq!(datetime_unit_quiet(6203, b"timezone_m"), Some(true));
        assert_eq!(datetime_unit_quiet(6199, b"hour"), Some(false), "not supported for date");
        assert_eq!(datetime_unit_quiet(6199, b"isodow"), Some(true));
        assert_eq!(datetime_unit_quiet(2021, b"fortnight"), Some(false));
        assert_eq!(datetime_unit_quiet(2020, b"day"), Some(true));
        assert_eq!(datetime_unit_quiet(2020, b"month"), Some(false), "below the earliest timestamp");
        assert_eq!(datetime_unit_quiet(2020, b"epoch"), Some(false), "DecodeUnits alone");
        assert_eq!(datetime_unit_quiet(1217, b"second"), Some(true));
        assert_eq!(datetime_unit_quiet(1217, b"hour"), Some(false), "an LMT offset");
        assert_eq!(datetime_unit_quiet(1258, b"day"), None);
    }

    #[test]
    fn const_arg_names_the_length_or_n_only_for_its_calls() {
        assert_eq!(const_arg(877, 3), Some(2));
        assert_eq!(const_arg(936, 3), Some(2));
        assert_eq!(const_arg(3060, 2), Some(1));
        assert_eq!(const_arg(3061, 2), Some(1));
        assert_eq!(const_arg(877, 2), None);
        assert_eq!(const_arg(156, 2), None);
        assert!(text_arg_quiet(877, Some(1)) && text_arg_quiet(3061, Some(-2)) && !text_arg_quiet(156, Some(7)));
    }
}

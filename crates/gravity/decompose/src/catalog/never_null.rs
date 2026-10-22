//! Never-NULL functions (D605): the functions whose result is never NULL when no argument is, each audited
//! against its PostgreSQL 18 C body (every return path returns a datum; a failure raises). A strict function is not
//! one of them by itself: `strict` promises NULL out for a NULL in, nothing for non-NULL in (substring(t FROM pattern),
//! regexp_match with no match; date_part / extract of a non-monotonic field of an infinite timestamp, PG >= 14), so a
//! function missing here is NULL-able.

/// The audited set, by `pg_proc` oid. Source: cpg-src src/backend/utils/adt (PostgreSQL 18); each function's every
/// return is `PG_RETURN_<type>(value)`, its only other exit an `ereport(ERROR)` (overflow, division by zero).
const AUDITED: &[u32] = &[
    141,  // int4mul                      int.c:848
    152,  // int2mul                      int.c:963
    154,  // int4div                      int.c:862 (division by zero raises)
    170,  // int24mul                     int.c:1045
    171,  // int42mul                     int.c:1106
    176,  // int2pl                       int.c:935
    177,  // int4pl                       int.c:820
    178,  // int24pl                      int.c:1017
    179,  // int42pl                      int.c:1078
    180,  // int2mi                       int.c:949
    181,  // int4mi                       int.c:834
    182,  // int24mi                      int.c:1031
    183,  // int42mi                      int.c:1092
    212,  // int4um                       int.c:800
    213,  // int2um                       int.c:915
    216,  // float8mul                    float.c:788
    217,  // float8div                    float.c:797 (division by zero raises)
    218,  // float8pl                     float.c:770
    219,  // float8mi                     float.c:779
    220,  // float8um                     float.c:669
    316,  // i4tod                        float.c:1264
    462,  // int8um                       int8.c:440
    463,  // int8pl                       int8.c:462
    464,  // int8mi                       int8.c:476
    465,  // int8mul                      int8.c:490
    466,  // int8div                      int8.c:504 (division by zero raises)
    480,  // int84 (int8 -> int4)         int8.c:1249 (out of range raises)
    481,  // int48 (int4 -> int8)         int8.c:1241
    482,  // i8tod                        int8.c:1283
    837,  // int82pl                      int8.c:1032
    838,  // int82mi                      int8.c:1046
    839,  // int82mul                     int8.c:1060
    841,  // int28pl                      int8.c:1113
    942,  // int28mi                      int8.c:1127
    943,  // int28mul                     int8.c:1141
    1257, // textlen (textlen)            varlena.c:702
    1274, // int84pl                      int8.c:890
    1275, // int84mi                      int8.c:904
    1276, // int84mul                     int8.c:918
    1277, // int84div                     int8.c:932 (division by zero raises)
    1278, // int48pl                      int8.c:971
    1279, // int48mi                      int8.c:985
    1280, // int48mul                     int8.c:999
    1281, // int48div                     int8.c:1013 (division by zero raises)
    1317, // textlen (length)             varlena.c:702
    1369, // textlen (character_length)   varlena.c:702
    1381, // textlen (char_length)        varlena.c:702
];

/// Whether function `funcid` returns non-NULL for non-NULL arguments (the audited set; the caller still checks that
/// every argument is never NULL).
pub fn audited(funcid: u32) -> bool {
    AUDITED.contains(&funcid)
}

#!/usr/bin/env python3
"""Generate Gravity's function tiers and the direct-shim families
(docs/gravity/expr.md §2.1): every pgrust builtin (fmgr_core::CANONICAL)
classified once, and for every tier-A/B row an `extern "C-unwind"` shim
that calls the builtin's TYPED BODY — the same Rust function the fmgr
wrapper calls — with no fcinfo and no palloc.

    python3 scripts/gen-gravity-function-tiers.py

writes crates/gravity/fn/function_tiers.tsv, crates/gravity/fn/src/tier.rs,
crates/gravity/fn/src/shims/{byval,byref_val,byref_ref,view_val,val_view,
catalog}.rs and prints the coverage report (counts per tier / family /
status, the typed bodies private to their crate, the wrappers the
translator could not read) on stderr; GRAVITY_TIERS_PENDING=1 lists every
pending row with its reason, GRAVITY_TIERS_CONTEXT=1 every context row.

How a shim is derived. The builtin's registry row (`b(oid, "name", nargs,
fc_x)` in the crate's `*_BUILTINS` table) names its fmgr wrapper; the
wrapper's body — hand-written, or the expansion of the `macro_rules!` list
it came from, macros nested to any depth — is translated token by token.
The translation is verbatim Rust except for the fmgr transport: reads of
the frame (`fcinfo.arg_i32(i)`, `arg_ptr(i)`, `arg_fixed(i, n)`,
`arg_varlena_packed(i)`, `args_n`, `get_collation()`, `result_mcx()`)
become the shim's arguments, local helpers and closures that take the
frame are inlined, `fmgr_seams::get_fn_expr_argtype(flinfo, i)` becomes a
leading resolved-type argument, `?` an early return with the error in the
caller's slot, and the final `PgResult<Datum>` (any shape: `Ok(Datum::
from_T(..))`, `byref_result`, `varlena_result`, `Datum::from_usize(p)`, a
`match` over `Some`/`None`, a bare `PgResult<Datum>` expression) is
finished into the shim's `Ret` — by value, or a copy of the result image
into the caller's region (an out function's `cstring_scratch(flinfo, name,
bytes)` too: the cstring goes into the region). A shim's text, bytea or
numeric argument is a payload view of an in-line uncompressed value, so
`arg_varlena_is_extended(i)` and the varatt header tests on its
`arg_ptr(i)` answer for that, and the toasted / short-header arms they
guard fold away with the other literal conditions (an `if` on a literal
bool keeps its taken branch; statements after a taken `return` are dead).
A wrapper that reads the call context, binds its
`FmgrInfo`, or reaches the catalog stays on the function manager with that
reason; one the translator cannot read stays `pending` (tests/tiers.rs
ratchets the count down, never up); a hand adapter in
shims/{text,numeric,datetime,jsonpath,array,network}.rs takes precedence when `HAND` names it.

Tiers (Michael's ruling, expr.md §6.1, pass 2): EVERY immutable or stable
builtin is a shim; the function-manager tier keeps only volatile,
set-returning, user-defined, window and genuinely context-dependent
functions (session/transaction state, catalog readers, the soft-error
context, FmgrInfo state across calls).
  A  open-coded reference (`OPEN_CODED`: the inline forms the kernels
     carry; the shim is the reference the inline form is tested against);
  B  direct shim: status `shim` (typed body, result by value or written
     into the caller's Region), `shim-copy` (the body allocates its result
     in an Mcx: private scratch context, one copy into the Region) or
     `pending`;
  C  function manager, with a reason from the closed vocabulary:
     volatile | set-returning | window | procedure | aggregate |
     handler/internal | polymorphic/record (OUT arguments, VARIADIC "any",
     an element-type result resolved per call) | non-strict | context
     (session/transaction state | catalog | soft-error context | FmgrInfo
     state across calls | fcinfo->context) | stub (no body ported).
Argument classes: by-value SQL types as `i64`; the text class (text /
varchar / bpchar / bytea / name / cstring) as a `View {ptr, len}` of the
payload; numeric / interval as a `View` of the payload; every other
by-reference type as a `View` of the detoasted IMAGE — a fixed-size type's
typlen bytes (uuid, macaddr, point, box, tid, timetz, …), a varlena's
4-byte-header image (arrays, jsonb, ranges, tsvector, bit, inet, …); a
polymorphic element (`anyelement`, `anycompatible`, `any`) as the Datum's
bits (by-value bits or the pointer to the detoasted image, per the resolved
type). Results by value, or copied into the Region (a payload, a fixed
image, a varlena image, a NUL-terminated cstring).
"""
import io
import os
import re
import sys
from collections import Counter, defaultdict

# `--check`: generate into memory and compare with the files in the tree; exit 1 naming each
# file that differs (tests/tiers.rs runs this so a stale table fails the crate's tests).
CHECK = '--check' in sys.argv
CHECK_STALE = []


class _Out(io.StringIO):
    def __init__(self, path):
        super().__init__()
        self.path = path

    def __exit__(self, *a):
        try:
            on_disk = open(self.path).read()
        except FileNotFoundError:
            on_disk = None
        if self.getvalue() != on_disk:
            CHECK_STALE.append(os.path.relpath(self.path, ROOT))
        return super().__exit__(*a)


def out_file(path):
    return _Out(path) if CHECK else open(path, 'w')

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REF = os.path.join(ROOT, 'crates/postgres-18.6-reference/src/include/catalog')
CANON = os.path.join(ROOT, 'crates/backend/utils/fmgr/fmgr_core/src/canonical.rs')
FN = os.path.join(ROOT, 'crates/gravity/fn')
CRATES = os.path.join(ROOT, 'crates')

# ---------------------------------------------------------------- catalogs


def parse_dat(path):
    """The rows of a .dat file: `{ key => 'value', .. }` blocks, braces inside quoted values ignored."""
    src = open(path).read()
    src = re.sub(r'^\s*#.*$', '', src, flags=re.M)
    rows, i, n = [], 0, len(src)
    while i < n:
        if src[i] != '{':
            i += 1
            continue
        j, depth, quoted = i, 0, False
        while j < n:
            c = src[j]
            if quoted:
                if c == '\\':
                    j += 1
                elif c == "'":
                    quoted = False
            elif c == "'":
                quoted = True
            elif c == '{':
                depth += 1
            elif c == '}':
                depth -= 1
                if depth == 0:
                    break
            j += 1
        row = dict(re.findall(r"(\w+)\s*=>\s*'((?:[^'\\]|\\.)*)'", src[i + 1:j], re.S))
        if row:
            rows.append(row)
        i = j + 1
    return rows


TYPES = {t['typname']: t for t in parse_dat(os.path.join(REF, 'pg_type.dat')) if 'typname' in t}
PROCS = {int(p['oid']): p for p in parse_dat(os.path.join(REF, 'pg_proc.dat')) if 'oid' in p and 'proname' in p}
CANONICAL = [(int(o), n, int(a), s == 'true', r == 'true') for o, n, a, s, r in re.findall(
    r'\(\s*(\d+),\s*"([^"]+)",\s*(\d+),\s*(true|false),\s*(true|false)\)', open(CANON).read())]

BYVAL = {
    'bool': 'bool', 'char': 'i8', 'int2': 'i16', 'int4': 'i32', 'int8': 'i64', 'oid': 'u32', 'xid': 'u32', 'cid': 'u32',
    'xid8': 'u64', 'float4': 'f32', 'float8': 'f64', 'money': 'i64', 'date': 'i32', 'time': 'i64', 'timestamp': 'i64',
    'timestamptz': 'i64', 'pg_lsn': 'u64',
}
for _t in ('regproc', 'regprocedure', 'regoper', 'regoperator', 'regclass', 'regcollation', 'regtype', 'regrole',
           'regnamespace', 'regconfig', 'regdictionary'):
    BYVAL[_t] = 'u32'
VIEW = {'text', 'varchar', 'bpchar', 'bytea', 'name', 'cstring'}
BYREF = {'numeric', 'interval'}
# a `void` result is a by-value zero datum (an ordinary host call: P1 item 2); a `void` argument (void_out / void_send) stays a handler row
HANDLER = {'internal', 'trigger', 'event_trigger', 'language_handler', 'fdw_handler', 'index_am_handler', 'tsm_handler',
           'table_am_handler', 'pg_ddl_command'}
POLY_IMG = {'anyarray', 'anycompatiblearray', 'anyrange', 'anycompatiblerange', 'anymultirange', 'anycompatiblemultirange',
            'record'}
POLY_ELEM = {'anyelement', 'anynonarray', 'anyenum', 'anycompatible', 'anycompatiblenonarray', 'any'}
POLY = POLY_IMG | POLY_ELEM | {'unknown'}


def typlen_of(tn):
    """The catalog typlen: -1 varlena, -2 cstring, n fixed."""
    if tn == 'unknown':
        return -2
    if tn.startswith('_') or tn in POLY_IMG:
        return -1
    r = TYPES.get(tn)
    if r is None:
        return None
    l = r.get('typlen', '-1')
    return {'NAMEDATALEN': 64, 'SIZEOF_POINTER': 8}.get(l, int(l) if re.match(r'^-?\d+$', l) else None)


# varatt header tests on a shim's payload view: the payload stands for an uncompressed in-line
# varlena with a 4-byte header (main's toasted / short-header arms are not taken)
VARATT_OF_PAYLOAD = {
    'varatt_is_1b_e': 'false', 'varatt_is_1b': 'false', 'varatt_is_short': 'false', 'varatt_is_external': 'false',
    'varatt_is_extended': 'false', 'varatt_is_compressed': 'false', 'varatt_is_4b_c': 'false',
    'varatt_is_4b': 'true', 'varatt_is_4b_u': 'true',
}


def arg_class(tn):
    """'byval' | 'view' | 'byref' (numeric/interval payload) | 'fixed' | 'img' | 'datum' | None (handler)."""
    if tn in BYVAL:
        return 'byval'
    if tn in VIEW or tn == 'unknown':
        return 'view'
    if tn in BYREF:
        return 'byref'
    if tn in HANDLER:
        return None
    if tn in POLY_ELEM:
        return 'datum'
    if tn in POLY_IMG:
        return 'img'
    if tn.startswith('_'):
        return 'img'
    r = TYPES.get(tn)
    if r is None:
        return None
    if r.get('typbyval') in ('t', 'FLOAT8PASSBYVAL'):
        return 'byval'
    tl = typlen_of(tn)
    if tl is None:
        return None
    if tl == -2:
        return 'view'
    if tl == -1:
        return 'img'
    return 'fixed'


def byval_ty(tn):
    if tn in BYVAL:
        return BYVAL[tn]
    r = TYPES[tn]
    return {'1': 'i8', '2': 'i16', '4': 'i32', '8': 'i64'}[r['typlen']]


# Rust value type -> (decode from the shim's i64, encode into Ret.val)
WIDEN = {
    'bool': ('({} != 0)', '({} as i64)'), 'i8': ('({} as i8)', '({} as i64)'), 'i16': ('({} as i16)', '({} as i64)'),
    'i32': ('({} as i32)', '({} as i64)'), 'i64': ('{}', '({})'), 'u32': ('({} as u32)', '({} as u32 as i64)'),
    'u64': ('({} as u64)', '({} as i64)'), 'f32': ('f32::from_bits({} as u32)', '({}.to_bits() as u32 as i64)'),
    'f64': ('f64::from_bits({} as u64)', '({}.to_bits() as i64)'), 'u16': ('({} as u16)', '({} as i64)'),
    'u8': ('({} as u8)', '({} as i64)'), 'usize': ('({} as usize)', '({} as i64)'),
}
GETTER = {'as_bool': 'bool', 'as_char': 'i8', 'as_i8': 'i8', 'as_i16': 'i16', 'as_i32': 'i32', 'as_i64': 'i64',
          'as_oid': 'u32', 'as_u32': 'u32', 'as_u64': 'u64', 'as_f32': 'f32', 'as_f64': 'f64', 'as_u16': 'u16',
          'as_u8': 'u8', 'as_usize': 'usize'}
CTOR = {'from_bool': 'bool', 'from_char': 'i8', 'from_i8': 'i8', 'from_i16': 'i16', 'from_i32': 'i32',
        'from_i64': 'i64', 'from_oid': 'u32', 'from_u32': 'u32', 'from_u64': 'u64', 'from_f32': 'f32', 'from_f64': 'f64',
        'from_u16': 'u16', 'from_u8': 'u8'}

# Builtins whose purpose is the session / transaction state: the function manager keeps them
# (ruling 6.1). Catalog readers are found by the crates their bodies reach (CATALOG_CRATES).
SESSION_NAMES = {
    'current_user', 'session_user', 'current_schema', 'current_schemas', 'current_database', 'current_query',
    'getdatabaseencoding', 'pg_client_encoding', 'pgsql_version', 'version', 'system_user', 'inet_client_addr',
    'inet_client_port', 'inet_server_addr', 'inet_server_port', 'now', 'statement_timestamp',
    'transaction_timestamp', 'current_date', 'current_time', 'current_timestamp', 'localtime',
    'localtimestamp', 'nextval', 'currval', 'lastval', 'setval', 'user', 'getpgusername',
    'current_setting', 'set_config', 'xid_age', 'mxid_age', 'row_security_active', 'get_current_ts_config',
    'pg_postmaster_start_time', 'pg_conf_load_time', 'tsquery_rewrite_query',
}
SESSION_PREFIXES = ('be_lo', 'pg_notification', 'pg_walfile', 'has_', 'txid_', 'pg_current_', 'pg_last_', 'pg_stat_', 'pg_backend_', 'pg_is_', 'pg_my_',
                    'pg_xact_', 'pg_jit_', 'pg_trigger_depth', 'pg_get_', 'pg_has_role', 'pg_notify', 'pg_listening',
                    'pg_event_trigger', 'pg_reload', 'pg_rotate', 'pg_terminate', 'pg_cancel', 'pg_sleep', 'pg_advisory',
                    'pg_try_advisory', 'pg_export_snapshot', 'pg_log_', 'pg_ls_', 'pg_read_', 'pg_stat', 'pg_control',
                    'pg_wal_summary', 'pg_wal_replay', 'pg_replication', 'pg_logical', 'pg_create_', 'pg_drop_', 'pg_copy_', 'pg_switch_',
                    'pg_promote', 'pg_backup', 'pg_start_', 'pg_stop_', 'pg_tablespace_', 'pg_database_', 'pg_relation_',
                    'pg_table_', 'pg_indexes_', 'pg_total_', 'pg_filenode', 'pg_column_',
                    'pg_options_', 'pg_describe_', 'pg_identify_', 'pg_sequence_', 'pg_nextoid', 'pg_partition_',
                    'pg_settings_', 'pg_show_', 'pg_prepared_', 'pg_cursor', 'pg_lock_', 'pg_blocking_', 'pg_safe_', 'show_config_',
                    'pg_isolation_', 'pg_available_', 'pg_hba_', 'pg_ident_', 'pg_timezone_', 'pg_tablespace',
                    'pg_input_', 'pg_import_', 'pg_collation_', 'pg_visible_', 'pg_config', 'pg_typeof', 'binary_upgrade_',
                    'pg_statistics_', 'pg_stats_', 'pg_num_', 'pg_client_', 'pg_encoding_', 'pg_convert', 'pg_conversion_',
                    'pg_opfamily_', 'pg_opclass_', 'pg_operator_', 'pg_function_', 'pg_type_', 'pg_proc_', 'pg_index_',
                    'pg_sequence', 'pg_extension_', 'pg_tablespace', 'pg_am', 'pg_ts_', 'pg_available', 'pg_wait_',
                    'pg_current', 'pg_split_', 'pg_basetype', 'pg_last', 'pg_get', 'pg_convert')
# Crates whose every wrapper is a catalog reader by construction (the row is classified before the
# translator reads it): records are deformed through the typcache; the XML mappers walk the catalog.
CATALOG_HOST_CRATES = {'adt_rowtypes': 'column types via the typcache', 'xmlmap': 'the schema mapping walks the catalog'}
# The crates whose use marks a catalog read. GUC reads (`guc_tables`), the process globals and
# interrupt checks (`init_small`), the OID constants crate (`catalog`) and size formatting
# (`dbsize::pg_size_bytes`) are not catalog reads: stable builtins read GUCs (timestamp_out,
# byteaout) and were shims in pass 1.
CATALOG_CRATES = {'lsyscache', 'syscache', 'syscache_seams', 'typcache', 'typcache_seams', 'fmgr_seams', 'format_type',
                  'namespace', 'namespace_seams', 'acl', 'acl_seams', 'regproc', 'regproc_seams', 'funcapi',
                  'relcache', 'heapam', 'tableam', 'indexam', 'pg_enum', 'pg_type_cat', 'catcache', 'ruleutils',
                  'ruleutils_seams', 'adt_regproc', 'ts_parse', 'ts_cache', 'miscinit', 'commands', 'parser',
                  'expandedrecord', 'expandedrecord_seams', 'tupdesc', 'heaptuple',
                  'scalar_seams', 'timestamp_seams', 'formatting_seams', 'domains', 'rowtypes', 'adt_rowtypes',
                  'adt_enum', 'sequence_seams', 'genfile', 'pgstatfuncs', 'lockfuncs', 'aclchk_seams', 'xmlmap'}

OPEN_CODED = set('''
extract_timestamp extract_date timestamp_part timestamp_trunc
int2pl int2mi int2mul int2div int4pl int4mi int4mul int4div int8pl int8mi int8mul int8div int24pl int24mi int24mul
int24div int42pl int42mi int42mul int42div int48pl int48mi int48mul int48div int84pl int84mi int84mul int84div
int28pl int28mi int28mul int28div int82pl int82mi int82mul int82div int2mod int4mod int8mod int2um int4um int8um
int2abs int4abs int8abs int2eq int2ne int2lt int2le int2gt int2ge int4eq int4ne int4lt int4le int4gt int4ge int8eq
int8ne int8lt int8le int8gt int8ge int24eq int24ne int24lt int24le int24gt int24ge int42eq int42ne int42lt int42le
int42gt int42ge int48eq int48ne int48lt int48le int48gt int48ge int84eq int84ne int84lt int84le int84gt int84ge
int28eq int28ne int28lt int28le int28gt int28ge int82eq int82ne int82lt int82le int82gt int82ge int2and int2or
int2xor int2not int2shl int2shr int4and int4or int4xor int4not int4shl int4shr int8and int8or int8xor int8not int8shl
int8shr int2larger int2smaller int4larger int4smaller int8larger int8smaller i2toi4 i4toi2 int48 int84 int28 int82
int24 int42 i4tod i8tod i2tod dtoi4 dtoi8 dtoi2 i4tof i8tof i2tof ftoi4 ftoi8 ftoi2 ftod dtof float4pl float4mi
float4mul float4div float8pl float8mi float8mul float8div float48pl float48mi float48mul float48div float84pl
float84mi float84mul float84div float4um float8um float4abs float8abs float4eq float4ne float4lt float4le float4gt
float4ge float8eq float8ne float8lt float8le float8gt float8ge float48eq float48ne float48lt float48le float48gt
float48ge float84eq float84ne float84lt float84le float84gt float84ge float4larger float4smaller float8larger
float8smaller booleq boolne boollt boolgt boolle boolge date_eq date_ne date_lt date_le date_gt date_ge date_pli
date_mii date_mi date_cmp timestamp_eq timestamp_ne timestamp_lt timestamp_le timestamp_gt timestamp_ge
timestamp_cmp timestamp_smaller timestamp_larger hashint2 hashint4 hashint8 hashfloat4 hashfloat8 hashbool
hashchar hashoid btint2cmp btint4cmp btint8cmp btint24cmp btint42cmp btint48cmp btint84cmp btint28cmp btint82cmp
btfloat4cmp btfloat8cmp btfloat48cmp btfloat84cmp btboolcmp btoidcmp btcharcmp oideq oidne oidlt oidle oidgt oidge
chareq charne charlt charle chargt charge textoctetlen byteaoctetlen bpcharoctetlen
'''.split())

HAND = {}
for _n in ('textcat text_substr text_substr_no_len textlike textnlike texticlike texticnlike hashtext hashtextextended '
           'int2out int4out int8out float4out float8out boolout').split():
    HAND[_n] = 'text'
for _n in 'nameiclike nameicnlike'.split():
    HAND[_n] = 'text'
for _n in 'numeric_larger numeric_smaller numeric_out numeric_in'.split():
    HAND[_n] = 'numeric'
for _n in ('jsonb_path_exists jsonb_path_exists_opr jsonb_path_exists_tz jsonb_path_match jsonb_path_match_opr '
           'jsonb_path_match_tz jsonb_path_query_array jsonb_path_query_array_tz jsonb_path_query_first '
           'jsonb_path_query_first_tz').split():
    HAND[_n] = 'jsonpath'
HAND['array_dims'] = 'array'
# `->>` reads its jsonb in place (shims/jsonb.rs)
HAND['jsonb_object_field_text'] = 'jsonb'
for _n in 'network_larger network_smaller'.split():
    HAND[_n] = 'network'
for _n in 'timestamp_out timestamptz_out date_out interval_out time_out timetz_larger timetz_smaller'.split():
    HAND[_n] = 'datetime'
# the transaction / statement start: the session state they read is the xact seam's, which a pool claim arms with the
# leader's pair (SessionCarry), so the hand shims read what the wrappers read on any thread
HAND_SESSION = {'now', 'statement_timestamp'}
for _n in HAND_SESSION:
    HAND[_n] = 'datetime'
# the one-argument input functions a CoerceViaIO cast calls per row: the wrappers read the soft-error context, a cast
# passes none (C InputFunctionCall), so the adapter calls the body with no context and the error is raised as PG's
for _n in 'int2in int4in int8in oidin boolin float4in float8in uuid_in date_in time_in timestamp_in timestamptz_in interval_in byteain inet_in cidr_in macaddr_in jsonb_in json_in'.split():
    HAND[_n] = 'input'
# A shim whose argument classes differ from its SQL types' (`arg_class`): substr takes its text or bytea as the raw
# Datum ('datum', ArgKind::D), so a compressed or external value is fetched only as far as the substring reaches
# (PostgreSQL's DatumGetTextPSlice / DatumGetByteaPSlice through text_substring's / bytea_substring's
# detoast_attr_slice), never detoasted whole at the call. HAND_ACLS for a hand adapter, GEN_ACLS for a generated body
# (its family and class column stay its SQL types').
HAND_ACLS = {'text_substr': ['datum', 'byval', 'byval'], 'text_substr_no_len': ['datum', 'byval'],
             'jsonb_object_field_text': ['datum', 'view']}
GEN_ACLS = {'bytea_substr': ['datum', 'byval', 'byval'], 'bytea_substr_no_len': ['datum', 'byval']}

# Wrappers the translator reads but whose shims cannot be built or are not shims by nature; each with
# the reason it is kept on the function manager (reviewed by hand, pass 2).
KEEP_C = {
    'aclitemout': 'context: catalog (role names)',
    # an index AM support procedure (amproc 12 of every GiST opclass; pg_amproc.dat), in the gist AM crate
    'gist_translate_cmptype_common': 'handler/internal: index AM support procedure (pg_amproc)',
    'postgresql_fdw_validator': 'context: catalog (FDW option validation reads the catalog)',
    'pg_indexam_has_property': 'context: catalog (index AM properties)',
    'pg_indexam_progress_phasename': 'context: catalog (index AM lookup)',
    # element comparison through the typcache (the ElemCmpState the wrapper caches is a catalog lookup)
    'array_eq': 'context: catalog (element comparison via the typcache)',
    'array_ne': 'context: catalog (element comparison via the typcache)',
    'array_lt': 'context: catalog (element comparison via the typcache)',
    'array_le': 'context: catalog (element comparison via the typcache)',
    'array_gt': 'context: catalog (element comparison via the typcache)',
    'array_ge': 'context: catalog (element comparison via the typcache)',
    'btarraycmp': 'context: catalog (element comparison via the typcache)',
    # headline generation needs the text search configuration (catalog) and its parser state
    # text search parsing and lexizing read the configuration, parser and dictionaries (catalog)
    'to_tsquery': 'context: catalog (text search configuration)',
    'to_tsquery_byid': 'context: catalog (text search configuration)',
    'plainto_tsquery': 'context: catalog (text search configuration)',
    'plainto_tsquery_byid': 'context: catalog (text search configuration)',
    'phraseto_tsquery': 'context: catalog (text search configuration)',
    'phraseto_tsquery_byid': 'context: catalog (text search configuration)',
    'websearch_to_tsquery': 'context: catalog (text search configuration)',
    'websearch_to_tsquery_byid': 'context: catalog (text search configuration)',
    'to_tsvector': 'context: catalog (text search configuration)',
    'to_tsvector_byid': 'context: catalog (text search configuration)',
    'jsonb_to_tsvector': 'context: catalog (text search configuration)',
    'jsonb_to_tsvector_byid': 'context: catalog (text search configuration)',
    'jsonb_string_to_tsvector': 'context: catalog (text search configuration)',
    'jsonb_string_to_tsvector_byid': 'context: catalog (text search configuration)',
    'json_to_tsvector': 'context: catalog (text search configuration)',
    'json_to_tsvector_byid': 'context: catalog (text search configuration)',
    'json_string_to_tsvector': 'context: catalog (text search configuration)',
    'json_string_to_tsvector_byid': 'context: catalog (text search configuration)',
    'ts_lexize': 'context: catalog (text search dictionaries)',
    'ts_headline': 'context: catalog (text search configuration)',
    'ts_headline_byid': 'context: catalog (text search configuration)',
    'ts_headline_opt': 'context: catalog (text search configuration)',
    'ts_headline_byid_opt': 'context: catalog (text search configuration)',
    'ts_headline_json': 'context: catalog (text search configuration)',
    'ts_headline_json_byid': 'context: catalog (text search configuration)',
    'ts_headline_json_byid_opt': 'context: catalog (text search configuration)',
    'ts_headline_json_opt': 'context: catalog (text search configuration)',
    'ts_headline_jsonb': 'context: catalog (text search configuration)',
    'ts_headline_jsonb_byid': 'context: catalog (text search configuration)',
    'ts_headline_jsonb_byid_opt': 'context: catalog (text search configuration)',
    'ts_headline_jsonb_opt': 'context: catalog (text search configuration)',
}
# Wrappers the translator reads but rustc rejects the shim of (a hand adapter is owed): pending.
PENDING_NAMES = {}

# The collatable SQL types: the shim takes `coll` whenever an argument is one.
COLLATABLE = {t for t, r in TYPES.items() if r.get('typcollation', '0') not in ('0',)}

# ---------------------------------------------------------------- tokens


TOKEN_RE = re.compile(r'''
    (?P<ws>\s+) | (?P<lc>//[^\n]*) | (?P<bc>/\*.*?\*/) |
    (?P<str>b?"(?:[^"\\]|\\.)*") | (?P<char>b?'(?:[^'\\]|\\.)') | (?P<life>'[A-Za-z_][A-Za-z0-9_]*) |
    (?P<num>-?\d[\dA-Za-z_.]*) | (?P<mv>\$[A-Za-z_][A-Za-z0-9_]*) | (?P<id>[A-Za-z_][A-Za-z0-9_]*) |
    (?P<op>::|->|=>|==|!=|<<=|>>=|<=|>=|<<|>>|\+=|-=|\*=|/=|%=|&=|\|=|\^=|&&|\|\||\.\.=|\.\.|[$!(){}\[\],;:.&|*+/%<>=?@#^~-])
''', re.X | re.S)


def tokenize(src):
    out = []
    for m in TOKEN_RE.finditer(src):
        k = m.lastgroup
        if k in ('ws', 'lc', 'bc'):
            continue
        out.append(m.group(k))
    # attributes `#[..]` / `#![..]` are not part of the expression grammar
    res, i = [], 0
    while i < len(out):
        if out[i] == '#' and i + 1 < len(out) and (out[i + 1] == '[' or (out[i + 1] == '!' and i + 2 < len(out) and out[i + 2] == '[')):
            j = i + 1 if out[i + 1] == '[' else i + 2
            i = group_end(out, j)
            continue
        res.append(out[i])
        i += 1
    return res


OPEN, CLOSE = {'(': ')', '[': ']', '{': '}'}, {')', ']', '}'}


def group_end(toks, i):
    """Index just past the balanced group opening at toks[i]."""
    depth, j = 0, i
    while j < len(toks):
        if toks[j] in OPEN:
            depth += 1
        elif toks[j] in CLOSE:
            depth -= 1
            if depth == 0:
                return j + 1
        j += 1
    return len(toks)


def split_toks(toks, sep):
    out, cur, i = [], [], 0
    while i < len(toks):
        if toks[i] in OPEN:
            j = group_end(toks, i)
            cur += toks[i:j]
            i = j
            continue
        if toks[i] == sep:
            out.append(cur)
            cur = []
        else:
            cur.append(toks[i])
        i += 1
    if cur:
        out.append(cur)
    return out


def split_args(toks):
    """Comma-split of call arguments; a closure's `|a, b|` parameter list is not a split point."""
    out, cur, i = [], [], 0
    while i < len(toks):
        t = toks[i]
        if t in OPEN:
            j = group_end(toks, i)
            cur += toks[i:j]
            i = j
            continue
        if t == '|' and (not cur or cur[-1] in ('move',)):
            j = i + 1
            while j < len(toks) and toks[j] != '|':
                j = group_end(toks, j) if toks[j] in OPEN else j + 1
            cur += toks[i:j + 1]
            i = j + 1
            continue
        if t == ',':
            out.append(cur)
            cur = []
        else:
            cur.append(t)
        i += 1
    if cur:
        out.append(cur)
    return [a for a in out if a]


def join(toks):
    s = ''
    for i, t in enumerate(toks):
        if i and re.match(r'[A-Za-z0-9_"\']', t[0]) and re.match(r'[A-Za-z0-9_"\')\]]', s[-1]):
            s += ' '
        elif i and t in ('as', 'if', 'else', 'match', 'return') or (i and s[-1] in ',;=' and t not in ',;'):
            s += ' '
        elif i and s.endswith(('as', 'if', 'else', 'return', 'match', 'let', 'mut', 'unsafe', 'in')) and re.match(r'[A-Za-z0-9_(&*!\-\[]', t[0]):
            s += ' '
        elif i and t in ('=', '=>', '==', '!=', '<=', '>=', '&&', '||', '+', '/', '%', '..', '..=', '->') and s[-1] != ' ':
            s += ' '
        elif i and s[-1:] in ('=', '>', '<', '&', '|', '+', '/', '%') and s[-2:] not in ('->', '::') and t not in ('=', '>', '&', '|', ',', ';', ')', ']'):
            s += ' '
        s += t
    return s


# ---------------------------------------------------------------- macro_rules! (the wrapper lists)


def parse_macro_defs(toks):
    """name -> [(pattern_tokens, body_tokens)] for every macro_rules! in toks."""
    defs = {}
    i = 0
    while i < len(toks) - 3:
        if toks[i] == 'macro_rules' and toks[i + 1] == '!':
            name = toks[i + 2]
            j = group_end(toks, i + 3)
            inner = toks[i + 4:j - 1]
            arms, k = [], 0
            while k < len(inner):
                if inner[k] in OPEN:
                    pe = group_end(inner, k)
                    pat = inner[k + 1:pe - 1]
                    if pe < len(inner) and inner[pe] == '=>':
                        be = group_end(inner, pe + 1)
                        arms.append((pat, inner[pe + 2:be - 1]))
                        k = be
                        continue
                k += 1
            defs[name] = arms
            i = j
        else:
            i += 1
    return defs


class NoMatch(Exception):
    pass


def match_pattern(pat, inp, binds):
    """Match macro pattern tokens against input tokens; fills binds (var -> token list or list of dicts for repetitions)."""
    pi = ii = 0
    while pi < len(pat):
        t = pat[pi]
        if t == '$' and pi + 1 < len(pat) and pat[pi + 1] == '(':
            pe = group_end(pat, pi + 1)
            sub = pat[pi + 2:pe - 1]
            sep = None
            k = pe
            if k < len(pat) and pat[k] not in ('*', '+', '?'):
                sep = pat[k]
                k += 1
            k += 1  # the repetition operator
            reps = []
            while ii < len(inp):
                b = {}
                try:
                    used = match_pattern(sub, inp[ii:], b)
                except NoMatch:
                    break
                if used == 0:
                    break
                reps.append(b)
                ii += used
                if sep is not None:
                    if ii < len(inp) and inp[ii] == sep:
                        ii += 1
                    else:
                        break
            binds['$rep'] = reps
            pi = k
            continue
        if t.startswith('$') and len(t) > 1 and pi + 2 < len(pat) and pat[pi + 1] == ':':
            frag = pat[pi + 2]
            if ii >= len(inp):
                raise NoMatch()
            if frag in ('ident', 'literal', 'lifetime'):
                binds[t] = [inp[ii]]
                ii += 1
            elif frag == 'tt':
                if inp[ii] in OPEN:
                    e = group_end(inp, ii)
                    binds[t] = inp[ii:e]
                    ii = e
                else:
                    binds[t] = [inp[ii]]
                    ii += 1
            else:  # expr / ty / path / block: up to the next literal pattern token at depth 0
                stop = pat[pi + 3] if pi + 3 < len(pat) and not pat[pi + 3].startswith('$') else None
                j = ii
                # a closure's parameter list (`|a: T, b| ...`, `move |a| ...`) is part of the expression
                if frag == 'expr' and j < len(inp) and (inp[j] == '|' or (inp[j] == 'move' and j + 1 < len(inp) and inp[j + 1] == '|')):
                    j += 2 if inp[j] == 'move' else 1
                    while j < len(inp) and inp[j] != '|':
                        j = group_end(inp, j) if inp[j] in OPEN else j + 1
                    j += 1
                while j < len(inp):
                    if inp[j] in OPEN:
                        j = group_end(inp, j)
                        continue
                    if stop is not None and inp[j] == stop:
                        break
                    if stop is None and inp[j] in (';', ','):
                        break
                    j += 1
                if j == ii:
                    raise NoMatch()
                b = inp[ii:j]
                if frag == 'expr' and b and (b[0] in ('|', 'move') or (b[0] == '||')):
                    b = ['('] + b + [')']
                binds[t] = b
                ii = j
            pi += 3
            continue
        if ii >= len(inp) or inp[ii] != t:
            raise NoMatch()
        pi += 1
        ii += 1
    return ii


def expand_body(body, binds):
    out, i = [], 0
    while i < len(body):
        t = body[i]
        if t == '$' and i + 1 < len(body) and body[i + 1] == '(':
            e = group_end(body, i + 1)
            sub = body[i + 2:e - 1]
            k = e
            sep = None
            if k < len(body) and body[k] not in ('*', '+', '?'):
                sep = body[k]
                k += 1
            k += 1
            for n, rep in enumerate(binds.get('$rep', [])):
                if n and sep:
                    out.append(sep)
                merged = dict(binds)
                merged.update(rep)
                out += expand_body(sub, merged)
            i = k
            continue
        if t.startswith('$') and t in binds:
            out += binds[t]
            i += 1
            continue
        out.append(t)
        i += 1
    return out


def expand_macro(defs, name, args):
    for pat, body in defs.get(name, []):
        binds = {}
        try:
            used = match_pattern(pat, args, binds)
        except NoMatch:
            continue
        if used != len(args):
            continue
        return expand_body(body, binds)
    return None


def expand_all(defs, toks, depth=0):
    """Expand every invocation of a local macro in toks, recursively (macros that emit macros)."""
    if depth > 8:
        return toks
    out, i, changed = [], 0, False
    while i < len(toks):
        if i + 2 < len(toks) and toks[i] in defs and toks[i + 1] == '!' and toks[i + 2] in OPEN and (i == 0 or toks[i - 1] != '!'):
            e = group_end(toks, i + 2)
            exp = expand_macro(defs, toks[i], toks[i + 3:e - 1])
            if exp is not None:
                out += exp
                changed = True
                i = e
                if i < len(toks) and toks[i] == ';':
                    i += 1
                continue
        out.append(toks[i])
        i += 1
    return expand_all(defs, out, depth + 1) if changed else out


# ---------------------------------------------------------------- source scan: wrappers by registry row


def crate_of(path):
    d = os.path.dirname(path)
    while d != CRATES and d != '/':
        c = os.path.join(d, 'Cargo.toml')
        if os.path.isfile(c):
            m = re.search(r'^\[package\]\s*\nname\s*=\s*"([^"]+)"', open(c).read(), re.M)
            return (m.group(1) if m else os.path.basename(d)), d
        d = os.path.dirname(d)
    return None, None


def use_map(toks, crate, module):
    """`use` resolution for a file: last segment (or alias) -> full path; nested groups included."""
    m = {}

    def full(p):
        if p == 'crate':
            return crate
        if p.startswith('crate::'):
            return crate + p[len('crate'):]
        if p.startswith('::'):
            return p[2:]
        if p.startswith('self::'):
            return crate + module + p[len('self'):]
        if p.startswith('super::'):
            parent = module.rsplit('::', 1)[0] if '::' in module else ''
            return crate + parent + p[len('super'):]
        return p

    def walk(base, item):
        if not item:
            return
        if '{' in item:
            bi = item.index('{')
            prefix = base + ''.join(item[:bi])
            for sub in split_toks(item[bi + 1:-1], ','):
                walk(prefix, sub)
            return
        if 'as' in item:
            ai = item.index('as')
            name, seg = ''.join(item[:ai]), item[ai + 1]
        else:
            name, seg = ''.join(item), item[-1]
        if item[-1] == '*':
            g = full((base + ''.join(item[:-1])).rstrip(':'))
            if g:
                m.setdefault('*', []).append(g)
            return
        if name == 'self':
            f = full(base.rstrip(':'))
            seg = base.rstrip(':').rsplit('::', 1)[-1]
        else:
            f = full(base + name)
        if f and seg != '_':
            m[seg] = f

    i = 0
    while i < len(toks):
        if toks[i] == 'extern' and i + 4 < len(toks) and toks[i + 1] == 'crate' and toks[i + 3] == 'as':
            m[toks[i + 4]] = toks[i + 2]
            i += 5
            continue
        if toks[i] == 'use' and (i == 0 or toks[i - 1] in (';', '}', '{', 'pub', ')')):
            j = i + 1
            while j < len(toks) and toks[j] != ';':
                j = group_end(toks, j) if toks[j] == '{' else j + 1
            walk('', toks[i + 1:j])
            i = j
        i += 1
    return m


def scan_fns(toks, fns, locals_=None):
    i = 0
    while i < len(toks):
        if toks[i] == 'fn' and i + 1 < len(toks) and (i == 0 or toks[i - 1] not in ('.', '::')):
            name = toks[i + 1]
            j = i + 2
            while j < len(toks) and toks[j] != '{' and toks[j] != ';':
                if toks[j] in OPEN and toks[j] != '{':
                    j = group_end(toks, j)
                    continue
                j += 1
            if j < len(toks) and toks[j] == '{':
                e = group_end(toks, j)
                sig = toks[i + 2:j]
                k = i - 1
                while k >= 0 and toks[k] in ('unsafe', 'const', 'async', 'extern', '"C"', '"C-unwind"'):
                    k -= 1
                vis = 'pub' if k >= 0 and toks[k] == 'pub' else ('pub(crate)' if k >= 3 and toks[k] == ')' and toks[k - 3] == 'pub' else 'private')
                fns.setdefault(name, (sig, toks[j + 1:e - 1], vis))
                if locals_ is not None:
                    locals_.add(name)
                i = e
                continue
        i += 1


def item_vis(toks, i):
    k = i - 1
    while k >= 0 and toks[k] in ('unsafe', 'const', 'async', 'extern', '"C"', '"C-unwind"'):
        k -= 1
    if k >= 0 and toks[k] == 'pub':
        return 'pub'
    if k >= 3 and toks[k] == ')' and toks[k - 3] == 'pub':
        return 'pub(crate)'
    return 'private'


def scan_newtypes(toks):
    """`struct Name(Field);` single-field tuple structs: name -> field type tokens."""
    out = {}
    for i, t in enumerate(toks[:-3]):
        if t == 'struct' and toks[i + 2] == '(' and (i == 0 or toks[i - 1] not in ('.', '::')):
            e = group_end(toks, i + 2)
            if e < len(toks) and toks[e] == ';':
                inner = toks[i + 3:e - 1]
                if inner and inner[0] == 'pub':
                    inner = inner[1:]
                if ',' not in inner:
                    out[toks[i + 1]] = inner
    return out


def scan_types(toks):
    """name -> visibility for the types and consts a token stream defines."""
    out = {}
    for i, t in enumerate(toks[:-1]):
        if t in ('struct', 'enum', 'type', 'union', 'trait') and re.match(r'^[A-Z]', toks[i + 1]) and (i == 0 or toks[i - 1] not in ('.', '::', 'dyn', 'impl', '<', ',')):
            out[toks[i + 1]] = item_vis(toks, i)
        if t in ('const', 'static') and i + 2 < len(toks) and toks[i + 2] == ':' and (i == 0 or toks[i - 1] not in ('unsafe', '*')):
            out[toks[i + 1]] = item_vis(toks, i)
    return out


def scan_statics(toks):
    """Thread-local statics: name -> type tokens; and file consts: name -> expr tokens."""
    tls, consts = {}, {}
    i = 0
    while i < len(toks):
        if toks[i] == 'static' and i + 3 < len(toks) and toks[i + 2] == ':':
            j = i + 3
            k = j
            while k < len(toks) and toks[k] not in ('=', ';'):
                k = group_end(toks, k) if toks[k] in OPEN else k + 1
            tls[toks[i + 1]] = toks[j:k]
            i = k
            continue
        if toks[i] == 'const' and i + 3 < len(toks) and toks[i + 2] == ':' and toks[i - 1] != 'unsafe' if i else False:
            k = i + 3
            while k < len(toks) and toks[k] != '=':
                k = group_end(toks, k) if toks[k] in OPEN else k + 1
            s = k + 1
            k = s
            while k < len(toks) and toks[k] != ';':
                k = group_end(toks, k) if toks[k] in OPEN else k + 1
            consts[toks[i + 1]] = toks[s:k]
            i = k
            continue
        i += 1
    return tls, consts


ALL_CRATES = {}
for _dp, _dn, _fn in os.walk(CRATES):
    _dn[:] = sorted(d for d in _dn if d not in ('target', 'postgres-18.6-reference'))
    if 'Cargo.toml' in _fn:
        _m = re.search(r'^\[package\]\s*\nname\s*=\s*"([^"]+)"', open(os.path.join(_dp, 'Cargo.toml')).read(), re.M)
        if _m:
            ALL_CRATES[_m.group(1)] = _dp
CRATE_ALIAS = {'types_fmgr': 'fmgr', 'alloc': 'std'}
# per-crate dependency renames (`numeric = { package = "adt_numeric" }`)
CRATE_RENAMES = {}
for _c, _d in ALL_CRATES.items():
    _t = open(os.path.join(_d, 'Cargo.toml')).read()
    CRATE_RENAMES[_c] = {a: pk for a, pk in re.findall(r'^([A-Za-z0-9_-]+)\s*=\s*\{[^}]*package\s*=\s*"([^"]+)"', _t, re.M) if a != pk}

FILES = {}       # path -> dict(toks, crate, cdir, uses, defs, fns, locals, module, tls, consts, types)
REGISTRY = {}    # oid -> (wrapper name, file)
CRATE_DIR = {}


def scan_sources():
    for dirpath, dirnames, filenames in os.walk(CRATES):
        dirnames[:] = sorted(d for d in dirnames if d not in ('target', 'postgres-18.6-reference', 'tests', 'gravity'))
        for f in sorted(filenames):
            if not f.endswith('.rs') or f == 'tests.rs':
                continue
            path = os.path.join(dirpath, f)
            try:
                raw = open(path).read()
            except UnicodeDecodeError:
                continue
            if 'fc_' not in raw and 'FmgrBuiltin' not in raw:
                continue
            crate, cdir = crate_of(path)
            if crate is None:
                continue
            CRATE_DIR[crate] = cdir
            toks = tokenize(raw)
            defs = parse_macro_defs(toks)
            fns = {}
            locals_ = set()
            scan_fns(toks, fns, locals_)
            # macro-generated items: expand every invocation of a local macro (nested to any depth)
            i = 0
            while i < len(toks) - 2:
                if toks[i] in defs and toks[i + 1] == '!' and toks[i + 2] in OPEN:
                    e = group_end(toks, i + 2)
                    exp = expand_all(defs, toks[i:e])
                    scan_fns(exp, fns)
                    i = e
                    continue
                i += 1
            rel = os.path.relpath(path, os.path.join(cdir, 'src'))[:-3]
            module = '' if rel in ('lib', 'main') else '::' + rel.replace('/', '::').removesuffix('::mod')
            tls, consts = scan_statics(toks)
            FILES[path] = dict(toks=toks, crate=crate, cdir=cdir, uses=use_map(toks, crate, module), defs=defs, fns=fns,
                               locals=locals_, module=module, tls=tls, consts=consts, types=scan_types(toks), newtypes=scan_newtypes(toks))
            for m in re.finditer(r'\b[a-z_]+\(\s*(\d+),\s*"([^"]+)",(?:\s*\d+,)?(?:\s*(?:true|false),){0,2}\s*([A-Za-z_][A-Za-z0-9_:]*)\s*,?\s*\)', raw):
                REGISTRY.setdefault(int(m.group(1)), (m.group(3), path))
            for m in re.finditer(r'FmgrBuiltin\s*\{[^{}]*?foid:\s*(\d+)[^{}]*?func:\s*([A-Za-z_][A-Za-z0-9_:]*)', raw, re.S):
                REGISTRY.setdefault(int(m.group(1)), (m.group(2), path))


scan_sources()

# ---------------------------------------------------------------- the translator


class Fail(Exception):
    pass


class Ctx(Exception):
    """The wrapper is context-dependent: the message is the C reason."""
    pass


KEYWORDS = {'as', 'break', 'const', 'continue', 'else', 'enum', 'false', 'fn', 'for', 'if', 'impl', 'in', 'let', 'loop',
            'match', 'mod', 'move', 'mut', 'pub', 'ref', 'return', 'self', 'Self', 'static', 'struct', 'super', 'trait',
            'true', 'type', 'unsafe', 'use', 'where', 'while', 'dyn'}
# local helpers of the shape `fn with_x_scratch<R>(f: impl FnOnce(Mcx) -> R) -> R` over a retained
# per-backend context: the shim's own scratch context takes the role
SCRATCH_HELPERS = {'with_exec_scratch'}
CONTEXT_METHODS = {'soft_error_context': 'soft-error context', 'error_save_node': 'soft-error context',
                   'agg_context': 'fcinfo->context', 'agg_state_node': 'fcinfo->context', 'context': 'fcinfo->context',
                   'resultinfo': 'fcinfo->context', 'rsinfo_mut': 'fcinfo->context', 'call_context': 'fcinfo->context',
                   'is_agg_context': 'fcinfo->context', 'WindowObject': 'fcinfo->context'}
FLINFO_PASS = {'expect', 'unwrap', 'as_ref', 'as_mut', 'as_deref', 'as_deref_mut'}
PRIM_TYPES = {'i8', 'i16', 'i32', 'i64', 'i128', 'isize', 'u8', 'u16', 'u32', 'u64', 'u128', 'usize', 'f32', 'f64', 'bool',
              'char', 'str', 'String', 'Vec', 'Box', 'Option', 'Some', 'None', 'Ok', 'Err', 'Result', 'Cow', 'Self'}


class Translator:
    """Wrapper body tokens -> shim body Rust text over `a{i}__`, `coll__`, `mcx__`, `e__`, `r__`, `t{i}__`."""

    def __init__(self, file, sqlargs, rettype, classes=None):
        self.f = file
        self.sqlargs = sqlargs
        self.classes = list(classes) if classes else [arg_class(t) for t in sqlargs]
        self.rettype = rettype
        self.rclass = arg_class(rettype)
        self.env = {}          # name -> (text, kind)
        self.uses_coll = False
        self.uses_mcx = False
        self.tyargs = set()    # arg indexes whose resolved type the wrapper reads; -1 = result type
        self.closure = 0       # closure nesting: `?` and `return` stay verbatim inside
        self.helper = []       # inlined-helper frames: dict(tail=bool, q=bool)
        self.depth = 0
        self.fresh = 0
        self.prelude = []      # statements hoisted before the body (thread-local scratch)
        self.extra = None      # the fn_extra type, held in a per-shim thread-local static
        self.uses_extra = False

    # -- names
    def gensym(self, base):
        self.fresh += 1
        return f'{base}_{self.fresh}__'

    def resolve(self, path):
        out = self.resolve_inner(path)
        segs = out.split('::')
        if len(segs) >= 2 and segs[0] in ALL_CRATES and re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', segs[-1]) and segs[-1] not in ('self',):
            # the defining module's visibility for the item, when the module can be found
            for cut in (len(segs) - 1, len(segs) - 2):
                if cut < 1:
                    break
                items = module_items('::'.join(segs[:cut]))
                nm = segs[cut]
                if nm in items:
                    if items[nm] != 'pub':
                        raise Fail('private item ' + nm)
                    break
        return out

    def resolve_inner(self, path):
        crate = self.f['crate']
        if path.startswith('crate::'):
            return crate + path[len('crate'):]
        if path.startswith('::'):
            head = path[2:].split('::')[0]
            head2 = CRATE_RENAMES.get(crate, {}).get(head, head)
            return CRATE_ALIAS.get(head2, head2) + path[2 + len(head):]
        if path.startswith('self::'):
            return crate + self.f['module'] + path[len('self'):]
        if path.startswith('super::'):
            parent = self.f['module'].rsplit('::', 1)[0] if '::' in self.f['module'] else ''
            return crate + parent + path[len('super'):]
        head = path.split('::')[0]
        if head in self.f['uses']:
            path = self.f['uses'][head] + path[len(head):]
            head = path.split('::')[0]
            head2 = CRATE_RENAMES.get(crate, {}).get(head, head)
            return CRATE_ALIAS.get(head2, head2) + path[len(head):]
        if head in CRATE_RENAMES.get(crate, {}):
            head2 = CRATE_RENAMES[crate][head]
            return CRATE_ALIAS.get(head2, head2) + path[len(head):]
        if head in CRATE_ALIAS:
            return CRATE_ALIAS[head] + path[len(head):]
        if head in PRIM_TYPES or head in ('core', 'std'):
            return path
        if head in ALL_CRATES:
            return CRATE_ALIAS.get(head, head) + path[len(head):]
        if head in self.f['types']:
            if self.f['types'][head] != 'pub':
                raise Fail('private item ' + head)
            return crate + self.f['module'] + '::' + path
        if head in self.f['fns']:
            if self.f['fns'][head][2] != 'pub':
                raise Fail('private fn ' + head)
            return crate + self.f['module'] + '::' + path
        if head in crate_root_items(crate):
            if crate_root_items(crate)[head] != 'pub':
                raise Fail('private item ' + head)
            return crate + '::' + path
        for g in self.f['uses'].get('*', []):
            if head in module_items(g):
                if module_items(g)[head] != 'pub':
                    raise Fail('private item ' + head)
                return g + '::' + path
        return path

    def resolve_types(self, toks):
        """Type / pattern tokens: paths resolved, lifetimes elided, the rest verbatim."""
        out, i = [], 0
        while i < len(toks):
            t = toks[i]
            if t.startswith("'") and len(t) > 1 and t[1] != '\\' and re.match(r"^'[A-Za-z_][A-Za-z0-9_]*$", t) and t != "'static":
                out.append("'_")
                i += 1
                continue
            if t == '::' and (i == 0 or not ((re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', toks[i - 1]) and toks[i - 1] not in KEYWORDS) or toks[i - 1] in ('>', '>>'))) and i + 1 < len(toks) and re.match(r'^[A-Za-z_]', toks[i + 1]):
                j = i + 1
                while j + 2 < len(toks) and toks[j + 1] == '::' and re.match(r'^[A-Za-z_]', toks[j + 2]):
                    j += 2
                out.append(self.resolve('::' + ''.join(toks[i + 1:j + 1])))
                i = j + 1
                continue
            if re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', t) and t not in KEYWORDS and (i == 0 or toks[i - 1] not in ('.', "'")):
                j = i
                while j + 2 < len(toks) + 1 and j + 1 < len(toks) and toks[j + 1] == '::' and j + 2 < len(toks) and re.match(r'^[A-Za-z_]', toks[j + 2]):
                    j += 2
                path = ''.join(toks[i:j + 1])
                if i + 1 < len(toks) and toks[i + 1] == ':' and not (i + 2 < len(toks) and toks[i + 2] == ':'):
                    out.append(t)  # a field / binding before `:`
                    i += 1
                    continue
                if '::' in path or t[0].isupper() or (t in self.f['uses'] and t not in self.env):
                    out.append(self.resolve(path) if not (i and toks[i - 1] == '::') else path)
                else:
                    out.append(self.env[t][0] if t in self.env and self.env[t][1] == 'val' else t)
                i = j + 1
                continue
            out.append(t)
            i += 1
        return join(out)

    # -- argument primitives
    def cls(self, i):
        if i >= len(self.sqlargs):
            raise Fail(f'argument {i} beyond the signature')
        return self.classes[i]

    def arg_byval(self, i, ty):
        if i >= len(self.sqlargs):
            return f'{{ let dead__: {ty} = unreachable!(); dead__ }}'
        c = self.cls(i)
        if c not in ('byval', 'datum'):
            raise Fail(f'arg {i} read by value but its type is {self.sqlargs[i]}')
        return WIDEN[ty][0].format(f'a{i}__')

    def arg_ptr(self, i):
        c = self.cls(i)
        if c in ('fixed', 'img') or (c == 'byref' and self.sqlargs[i] == 'interval'):
            return f'a{i}__.ptr'
        if c == 'datum':
            return f'(a{i}__ as usize as *const u8)'
        raise Fail(f'arg {i} read as a raw pointer but its type is {self.sqlargs[i]} (a payload view)')

    def arg_index(self, args):
        if len(args) < 1:
            raise Fail('argument index missing')
        v, k = self.expr(args[0])
        t = re.sub(r'(\d)(usize|isize|u8|u16|u32|u64|i8|i16|i32|i64)', r'\1', v.replace(' ', '').replace('asusize', '').replace('asi16', ''))
        if k != 'val' or not re.match(r'^[\d()+\-*]+$', t):
            raise Fail('argument index is not a literal: ' + v)
        return int(eval(t))

    # -- expressions
    def expr(self, toks):
        """-> (text, kind); kind: 'val' | 'noq' | 'fcinfo' | 'flinfo' | 'mcx' | 'coll' | 'null' | ('nd', i) | ('ndv', i) | ('packed', i) | ('cstr', i) | ('tl', name)"""
        toks = list(toks)
        if not toks:
            raise Fail('empty expression')
        t = toks[0]
        if t in ('if', 'match', 'unsafe', 'return', 'loop', 'while', 'for', 'break', 'continue') or (t == '{' and group_end(toks, 0) == len(toks)):
            return self.prefix(toks)
        if t in ('|', '||', 'move'):
            return self.closure_expr(toks), 'val'
        # a prefix construct (`if`, `match`, ..) owns everything after it: never split inside one
        limit, depth = len(toks), 0
        for n, tk in enumerate(toks):
            if tk in OPEN:
                depth += 1
            elif tk in CLOSE:
                depth -= 1
            elif depth == 0 and tk in ('if', 'match', 'unsafe', 'loop', 'while', 'for', 'return', 'break', 'continue', '|', '||', 'move') and n > 0:
                if tk in ('|', '||') and not (toks[n - 1] in ('(', ',', '=', '=>', '{', ';') or n == 0):
                    continue
                limit = n
                break
        if toks[0] in ('..', '..='):
            return toks[0] + (self.expr(toks[1:])[0] if len(toks) > 1 else ''), 'val'
        if toks[-1] in ('..', '..=') and len(toks) > 1:
            return self.expr(toks[:-1])[0] + toks[-1], 'val'
        # binary operators, lowest precedence first
        for ops in (('=', '+=', '-=', '*=', '/=', '|=', '&=', '%=', '^=', '<<=', '>>='), ('..', '..='), ('||',), ('&&',), ('==', '!=', '<', '>', '<=', '>='), ('|',), ('^',), ('&',), ('<<', '>>'), ('+', '-'), ('*', '/', '%')):
            depth, i = 0, len(toks) - 1
            while i > 0:
                t = toks[i]
                if t in CLOSE:
                    depth += 1
                elif t in OPEN:
                    depth -= 1
                elif depth == 0 and t in ops and i < limit:
                    prev = toks[i - 1]
                    if not (re.match(r'^(b?"|b?\'|[A-Za-z0-9_])', prev) or prev in (')', ']', '}', '?')) or prev in KEYWORDS - {'self', 'true', 'false'}:
                        i -= 1
                        continue
                    if t in ('<', '>') and self.angle_generic(toks, i):
                        i -= 1
                        continue
                    if t == '=' and i + 1 < len(toks) and toks[i + 1] in ('=', '>'):
                        i -= 1
                        continue
                    if t == '&' and i + 1 < len(toks) and toks[i + 1] in ('&', 'mut'):
                        i -= 1
                        continue
                    if t == '|' and i + 1 < len(toks) and toks[i + 1] == '|':
                        i -= 1
                        continue
                    l, lk = self.expr(toks[:i])
                    if t in ('=', '+=', '-=', '*=', '/=', '|=', '&=', '%=') and lk == 'fcinfo':
                        raise Fail('assignment to the frame')
                    if t in ('||', '&&'):
                        lb = self.lit_bool(l)
                        if lb == ('true' if t == '||' else 'false'):
                            return lb, 'val'
                        if lb is not None:
                            return self.expr(toks[i + 1:])
                    r, _ = self.expr(toks[i + 1:])
                    if t in ('||', '&&') and self.lit_bool(r) == ('false' if t == '||' else 'true'):
                        return l, 'val'
                    if t in ('||', '&&') and self.lit_bool(r) is not None and '?' not in l and not re.search(r'[A-Za-z0-9_]\s*[(!]', l):
                        return self.lit_bool(r), 'val'
                    return f'({l} {t} {r})' if t not in ('..', '..=') else f'({l}{t}{r})', 'val'
                i -= 1
        # `as` binds tighter than binary operators
        depth, i = 0, len(toks) - 1
        while i > 0:
            t = toks[i]
            if t in CLOSE:
                depth += 1
            elif t in OPEN:
                depth -= 1
            elif depth == 0 and t == 'as' and i < limit:
                l, _ = self.expr(toks[:i])
                return f'({l} as {self.resolve_types(toks[i + 1:])})', 'val'
            i -= 1
        t = toks[0]
        if t == '(' and group_end(toks, 0) == len(toks):
            parts = split_toks(toks[1:-1], ',')
            if len(parts) == 1 and toks[-2] != ',':
                return self.expr(parts[0])
            return '(' + ', '.join(self.expr(p)[0] for p in parts) + (',' if len(parts) == 1 else '') + ')', 'val'
        if t == '[' and group_end(toks, 0) == len(toks):
            inner = toks[1:-1]
            parts = split_toks(inner, ';')
            if len(parts) == 2 and ';' in inner:
                return f'[{self.expr(parts[0])[0]}; {self.expr(parts[1])[0]}]', 'val'
            return '[' + ', '.join(self.expr(p)[0] for p in split_toks(inner, ',') if p) + ']', 'val'
        if t in ('&', '!', '*', '-') and len(toks) > 1:
            if t == '&' and toks[1] == 'mut':
                v, _ = self.expr(toks[2:])
                return '&mut ' + v, 'val'
            v, k = self.expr(toks[1:])
            if k != 'val' and k != 'noq':
                if t == '&' and k == 'mcx':
                    return '&mcx__', 'mcx'
                if t == '*' and isinstance(k, tuple) and k[0] == 'nd':
                    return v, k
                raise Fail(f'unary {t} on {k}')
            if t == '!' and self.lit_bool(v) is not None:
                return ('false' if self.lit_bool(v) == 'true' else 'true'), 'val'
            return t + v, 'val'
        return self.postfix(toks)

    def angle_generic(self, toks, i):
        """toks[i] is `<` or `>`: part of a turbofish / generic list rather than a comparison?"""
        if toks[i] == '<':
            return i > 0 and toks[i - 1] == '::' or (i > 0 and toks[i - 1][0].isupper())
        depth = 0
        for j in range(i, -1, -1):
            if toks[j] == '>':
                depth += 1
            elif toks[j] == '<':
                depth -= 1
                if depth == 0:
                    return self.angle_generic(toks, j)
        return False

    def closure_expr(self, toks):
        """`|params| body` or `move |params| body` — translated with the params bound as plain names."""
        i = 0
        if toks[0] == 'move':
            i = 1
        if toks[i] == '||':
            params, j = [], i + 1
        else:
            j = toks.index('|', i + 1)
            params = split_toks(toks[i + 1:j], ',')
            j += 1
        body = toks[j:]
        if body and body[0] == '->':
            k = 1
            while body[k] != '{':
                k += 1
            ret = ' -> ' + self.resolve_types(body[1:k])
            body = body[k:]
        else:
            ret = ''
        saved = dict(self.env)
        for p in params:
            for n in self.pattern_names(p):
                self.env[n] = (n, 'val')
        self.closure += 1
        try:
            text = self.expr(body)[0]
        finally:
            self.closure -= 1
            self.env = saved
        ps = ', '.join(self.resolve_types(p) for p in params)
        return f'{"move " if toks[0] == "move" else ""}|{ps}|{ret} {text}'

    def pattern_names(self, toks):
        out = []
        for i, t in enumerate(toks):
            if re.match(r'^[a-z_][A-Za-z0-9_]*$', t) and t not in KEYWORDS and (i + 1 >= len(toks) or toks[i + 1] != '::') and (i == 0 or toks[i - 1] != '::'):
                if t in ('e', 'r', 'coll', 'mcx', 'args') or t.endswith('__'):
                    toks[i] = t + '_v__'
                    self.env[t] = (t + '_v__', 'val')
                    continue
                out.append(t)
        return out

    def prefix(self, toks):
        t = toks[0]
        if t == 'unsafe' and toks[1] == '{':
            ge = group_end(toks, 1)
            if ge < len(toks) and toks[ge] in ('&', '|', '^', '&&', '||', '==', '!=', '<', '>', '<=', '>=', '+', '-', '*', '/', '%', '<<', '>>') and ';' not in toks[2:ge - 1] and not self.has_let(toks[2:ge - 1]):
                return self.expr(['('] + toks[2:ge - 1] + [')'] + toks[ge:])
            inner = self.expr(toks[2:ge - 1]) if ';' not in toks[2:ge - 1] and not self.has_let(toks[2:ge - 1]) else ('{ ' + self.block_value(toks[2:ge - 1]) + ' }', 'val')
            if ge == len(toks):
                return inner
            return self.postfix_on(inner[0], inner[1], toks[ge:])
        if t == '{':
            return '{ ' + self.block_value(toks[1:-1]) + ' }', 'val'
        if t == 'if':
            cast = self.if_value_with_cast(toks)
            if cast is not None:
                return cast, 'val'
            return self.if_expr(toks, False), 'val'
        if t == 'match':
            return self.match_expr(toks, False), 'val'
        if t == 'return':
            if self.closure:
                return 'return ' + (self.expr(toks[1:])[0] if len(toks) > 1 else ''), 'val'
            return 'return ' + self.ret_text(toks[1:] if len(toks) > 1 else ['(', ')']), 'val'
        if t in ('break', 'continue'):
            return join(toks), 'val'
        if t == 'loop':
            return 'loop { ' + self.block_value(toks[2:-1]) + ' }', 'val'
        if t == 'while':
            j = self.find_brace(toks, 1)
            cond = self.cond(toks[1:j])
            return f'while {cond} {{ {self.block_value(toks[j + 1:group_end(toks, j) - 1])} }}', 'val'
        if t == 'for':
            k = toks.index('in')
            j = self.find_brace(toks, k + 1)
            it = self.expr(toks[k + 1:j])[0]
            for n in self.pattern_names(toks[1:k]):
                self.env[n] = (n, 'val')
            return f'for {self.resolve_types(toks[1:k])} in {it} {{ {self.block_value(toks[j + 1:group_end(toks, j) - 1])} }}', 'val'
        raise Fail('prefix ' + t)

    def has_let(self, toks):
        depth = 0
        for t in toks:
            if t in OPEN:
                depth += 1
            elif t in CLOSE:
                depth -= 1
            elif depth == 0 and t == 'let':
                return True
        return False

    def find_brace(self, toks, j):
        while toks[j] != '{':
            if toks[j] in OPEN:
                j = group_end(toks, j)
                continue
            if toks[j] == 'unsafe' and j + 1 < len(toks) and toks[j + 1] == '{':
                j = group_end(toks, j + 1)
                continue
            j += 1
        return j

    def cond(self, toks):
        """An `if` / `while` condition, including `let PAT = EXPR` forms."""
        if toks[0] == 'let':
            eq = toks.index('=')
            rhs, k = self.expr(toks[eq + 1:])
            for n in self.pattern_names(toks[1:eq]):
                self.env[n] = (n, 'val')
            pat = self.resolve_types(toks[1:eq])
            if k == 'flinfo':
                raise Ctx('FmgrInfo state across calls (`if let` over flinfo)')
            if k != 'val' and k != 'noq':
                raise Fail(f'if let over {k}')
            return f'let {pat} = {rhs}'
        parts = split_toks(toks, '&&')
        if len(parts) > 1 and any(p and p[0] == 'let' for p in parts):
            return ' && '.join(self.cond(p) for p in parts)
        v, k = self.expr(toks)
        if k != 'val' and k != 'noq':
            raise Fail(f'condition of kind {k}')
        return v

    def lit_bool(self, text):
        """A translated expression that is a bool literal under parens, blocks, `unsafe` and an
        inlined helper's `let out__: bool = ..; out__`: 'true' / 'false', or None."""
        try:
            c = tokenize(text)
        except Exception:
            return None
        while c:
            if c in (['true'], ['false']):
                return c[0]
            if c[0] in ('(', '{') and group_end(c, 0) == len(c):
                c = c[1:-1]
            elif c[0] == 'unsafe' and len(c) > 1 and c[1] == '{' and group_end(c, 1) == len(c):
                c = c[2:-1]
            elif c[:5] == ['let', 'out__', ':', 'bool', '='] and c[-2:] == [';', 'out__']:
                c = c[5:-2]
            elif c[0] == 'use' and ';' in c:
                c = c[c.index(';') + 1:]
            else:
                return None
        return None

    def fold_if(self, toks):
        """`if true {A} else {B}` / `if false ..` -> the taken branch (a block or an `if`), or None."""
        j = self.find_brace(toks, 1)
        c = toks[1:j]
        if c and c[0] != 'let':
            saved_env, saved_mcx, saved_coll = dict(self.env), self.uses_mcx, self.uses_coll
            try:
                ctext = self.cond(c)
            except (Fail, Ctx):
                ctext = None
            self.env, self.uses_mcx, self.uses_coll = saved_env, saved_mcx, saved_coll
            if ctext is not None:
                c = tokenize(ctext)
                lb = self.lit_bool(ctext)
                if lb is not None:
                    c = [lb]
        while len(c) >= 2 and c[0] == '(' and group_end(c, 0) == len(c):
            c = c[1:-1]
        m = re.match(r'^(-?\d+) (>=|<=|==|!=|>|<) (-?\d+)$', ' '.join(c))
        if m:
            a, op, b = int(m.group(1)), m.group(2), int(m.group(3))
            val = {'>=': a >= b, '<=': a <= b, '==': a == b, '!=': a != b, '>': a > b, '<': a < b}[op]
            c = ['true' if val else 'false']
        if c not in (['true'], ['false']):
            return None
        te = group_end(toks, j)
        if c == ['true']:
            return toks[j:te]
        if te < len(toks) and toks[te] == 'else':
            if toks[te + 1] == 'if':
                return toks[te + 1:]
            return toks[te + 1:group_end(toks, te + 1)]
        return ['{', '}']

    def fold_match(self, toks):
        """`match X { .. }` with X a literal `None` / `Some(lit)` / integer / bool bound in the
        environment: the taken arm's tokens (bindings entered), or None."""
        j = self.find_brace(toks, 1)
        scrut = toks[1:j]
        if len(scrut) == 1 and scrut[0] in self.env and self.env[scrut[0]][1] == 'val':
            scrut = tokenize(self.env[scrut[0]][0])
        while len(scrut) >= 2 and scrut[0] == '(' and group_end(scrut, 0) == len(scrut):
            scrut = scrut[1:-1]
        lit = None
        if scrut == ['None'] or scrut in (['true'], ['false']) or (len(scrut) == 1 and re.match(r'^-?\d', scrut[0])):
            lit = scrut
        elif len(scrut) >= 3 and scrut[0] == 'Some' and scrut[1] == '(' and group_end(scrut, 1) == len(scrut):
            lit = scrut
        if lit is None:
            return None
        inner = toks[j + 1:-1]
        i = 0
        while i < len(inner):
            a = i
            while a < len(inner) and inner[a] != '=>':
                a = group_end(inner, a) if inner[a] in OPEN else a + 1
            pat = inner[i:a]
            b = a + 1
            if inner[b] == '{':
                be = group_end(inner, b)
                body = inner[b:be]
                nxt = be + 1 if be < len(inner) and inner[be] == ',' else be
            else:
                be = b
                while be < len(inner) and inner[be] != ',':
                    be = group_end(inner, be) if inner[be] in OPEN else be + 1
                body = inner[b:be]
                nxt = be + 1
            if 'if' in pat:
                return None
            take = False
            if pat == ['_']:
                take = True
            elif lit[0] == 'None' and pat == ['None']:
                take = True
            elif lit[0] == 'Some' and len(pat) >= 3 and pat[0] == 'Some' and pat[1] == '(':
                inner_pat = pat[2:-1]
                if len(inner_pat) == 1 and re.match(r'^[a-z_][A-Za-z0-9_]*$', inner_pat[0]):
                    self.env[inner_pat[0]] = (join(lit[2:-1]), 'val')
                    take = True
                elif inner_pat == ['_']:
                    take = True
            elif lit[0] in ('true', 'false') and pat == lit:
                take = True
            elif re.match(r'^-?\d', lit[0]) and pat == lit:
                take = True
            elif len(pat) == 1 and re.match(r'^[a-z_][A-Za-z0-9_]*$', pat[0]) and pat[0] not in ('None',):
                self.env[pat[0]] = (join(lit), 'val')
                take = True
            if take:
                return body
            i = nxt
        return None

    def if_expr(self, toks, ret, ok=False):
        """`if c { } else if .. else { }`; in ret mode every branch tail is finished into a Ret."""
        folded = self.fold_if(toks)
        if folded is not None and not (not ret and folded[1:-1] == ['None']):
            if folded[0] == 'if':
                return self.if_expr(folded, ret, ok)
            inner = folded[1:-1]
            if ret:
                return self.block_ret(inner, ok)
            return '{ ' + self.block_value(inner) + ' }'
        j = self.find_brace(toks, 1)
        cond = self.cond(toks[1:j])
        te = group_end(toks, j)
        then = self.block_ret(toks[j + 1:te - 1], ok) if ret else self.block_value(toks[j + 1:te - 1])
        if te < len(toks) and toks[te] == 'else':
            if toks[te + 1] == 'if':
                return f'if {cond} {{ {then} }} else {{ {self.if_expr(toks[te + 1:], ret, ok)} }}'
            ee = group_end(toks, te + 1)
            els = self.block_ret(toks[te + 2:ee - 1], ok) if ret else self.block_value(toks[te + 2:ee - 1])
            rest = toks[ee:]
            out = f'if {cond} {{ {then} }} else {{ {els} }}'
            if rest:
                raise Fail('tokens after if/else')
            return out
        if ret:
            raise Fail('if without else in result position')
        if te != len(toks):
            raise Fail('tokens after if')
        return f'if {cond} {{ {then} }}'

    def if_value_with_cast(self, toks):
        """`if c {a} else {b} as T` (an if-expression the wrapper casts)."""
        j = self.find_brace(toks, 1)
        te = group_end(toks, j)
        while te < len(toks) and toks[te] == 'else':
            if toks[te + 1] == 'if':
                te = group_end(toks, self.find_brace(toks, te + 2))
            else:
                te = group_end(toks, te + 1)
        if te < len(toks) and toks[te] == 'as':
            return f'({self.if_expr(toks[:te], False)} as {self.resolve_types(toks[te + 1:])})'
        return None

    def match_expr(self, toks, ret, ok=False):
        folded = self.fold_match(toks)
        if folded is not None:
            if folded[0] == '{' and group_end(folded, 0) == len(folded):
                if ret:
                    return self.block_ret(folded[1:-1], ok)
                return '{ ' + self.block_value(folded[1:-1]) + ' }'
            if ret:
                return self.ret_ok(folded) if ok else self.ret_text(folded)
            return self.expr(folded)[0]
        j = self.find_brace(toks, 1)
        scrut, k = self.expr(toks[1:j])
        if k == 'flinfo':
            raise Ctx('FmgrInfo state across calls (match over flinfo)')
        if k not in ('val', 'noq'):
            raise Fail(f'match over {k}')
        if group_end(toks, j) != len(toks):
            raise Fail('tokens after match')
        inner = toks[j + 1:-1]
        arms, i = [], 0
        while i < len(inner):
            # pattern [if guard] => body ,
            a = inner.index('=>', i)
            depth, a = 0, i
            while a < len(inner):
                if inner[a] in OPEN:
                    a = group_end(inner, a)
                    continue
                if inner[a] == '=>':
                    break
                a += 1
            pat = inner[i:a]
            guard = ''
            if 'if' in pat:
                gi = pat.index('if')
                pat, gtoks = pat[:gi], pat[gi + 1:]
            else:
                gtoks = None
            saved_env = dict(self.env)
            for n in self.pattern_names(pat):
                self.env[n] = (n, 'val')
            if gtoks is not None:
                guard = ' if ' + self.cond(gtoks)
            b = a + 1
            if inner[b] == '{':
                be = group_end(inner, b)
                body_toks = inner[b:be]
                nxt = be + 1 if be < len(inner) and inner[be] == ',' else be
            else:
                be = b
                while be < len(inner):
                    if inner[be] in OPEN:
                        be = group_end(inner, be)
                        continue
                    if inner[be] == ',':
                        break
                    be += 1
                body_toks = inner[b:be]
                nxt = be + 1
            if ret:
                if body_toks[0] == '{' and group_end(body_toks, 0) == len(body_toks):
                    body = self.block_ret(body_toks[1:-1], ok)
                else:
                    body = self.ret_ok(body_toks) if ok else self.ret_text(body_toks)
            else:
                body = self.expr(body_toks)[0]
            arms.append(f'{self.resolve_types(pat)}{guard} => {{ {body} }}')
            self.env = saved_env
            i = nxt
        return f'match {scrut} {{ {", ".join(arms)} }}'

    def postfix(self, toks):
        i = 0
        if toks[0] == '(':
            i = group_end(toks, 0)
            base, kind = self.expr(toks[1:i - 1])
            if kind in ('val', 'noq'):
                base = '(' + base + ')'
            # an immediately invoked closure
            if i < len(toks) and toks[i] == '(' and toks[1] in ('|', '||', 'move'):
                e = group_end(toks, i)
                args = split_args(toks[i + 1:e - 1])
                base, kind = self.inline_closure(toks[1:i - 1], args), 'val'
                i = e
        elif re.match(r'^(b?"|b?\'|-?\d|true$|false$)', toks[0]):
            base, kind = toks[0], 'val'
            i = 1
        else:
            j = 0
            while j < len(toks) and (re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', toks[j]) or toks[j] == '::'):
                if toks[j] == '::' and j + 1 < len(toks) and toks[j + 1] == '<':
                    j = group_end_angle(toks, j + 1)
                    continue
                j += 1
            raw_path = toks[:j]
            path = re.sub(r'::<.*?>', '', ''.join(raw_path))
            turbofish = ''
            called = False
            if '<' in ''.join(raw_path):
                k = raw_path.index('<') - 1
                turbofish = '::' + self.resolve_types(raw_path[k + 1:group_end_angle(raw_path, k + 1)])
                path = ''.join(raw_path[:k]) + ''.join(raw_path[group_end_angle(raw_path, k + 1):])
            i = j
            if i < len(toks) and toks[i] == '!':
                if i + 1 < len(toks) and toks[i + 1] in OPEN:
                    e = group_end(toks, i + 1)
                    base, kind = self.macro(path, toks[i + 2:e - 1])
                    called = True
                    i = e
                else:
                    raise Fail('macro ' + path)
            elif i < len(toks) and toks[i] == '(' and not (path in self.env and self.env[path][1] != 'val') and not (path in self.env and path in ('Some', 'Ok', 'Err')):
                e = group_end(toks, i)
                args = split_args(toks[i + 1:e - 1])
                saved_q = self.questioned
                self.questioned = e < len(toks) and toks[e] == '?'
                try:
                    base, kind = self.call(path, args, tail=False, tf=turbofish)
                finally:
                    self.questioned = saved_q
                called = True
                i = e
            elif i < len(toks) and toks[i] == '{' and path.split('::')[-1][:1].isupper() and path not in ('Some', 'Ok', 'Err') and self.struct_literal_ahead(toks, i):
                e = group_end(toks, i)
                base, kind = self.struct_literal(path, toks[i + 1:e - 1]), 'val'
                i = e
            elif path in self.env:
                base, kind = self.env[path]
            elif path == 'fcinfo':
                base, kind = 'fcinfo', 'fcinfo'
            elif path in self.f['tls']:
                base, kind = path, ('tl', path)
            elif path in self.f['consts']:
                base, kind = '(' + self.expr(self.f['consts'][path])[0] + ')', 'val'
            elif path in ('None', 'Some', 'Ok', 'Err', 'true', 'false', 'self'):
                base, kind = path, 'val'
            elif not path:
                raise Fail('cannot parse `' + join(toks[:10]) + '`')
            elif '::' in path or path[0].isupper():
                base, kind = self.resolve(path), 'val'
            elif path in self.f['uses'] and path not in self.env:
                base, kind = self.resolve(path), 'val'
            elif path in self.f['fns']:
                sig, _, vis = self.f['fns'][path]
                if vis != 'pub' or self.datum_returning(sig):
                    params, _ = self.params_of(sig)
                    names = [self.gensym('p') for _ in params]
                    text, _ = self.inline_body(path, params, [[n] for n in names], self.f['fns'][path][1], False, self.has_return(self.f['fns'][path][1]))
                    base, kind = f'(|{", ".join(names)}| {text})', 'val'
                else:
                    base, kind = self.resolve(path), 'val'
            else:
                base, kind = path, 'val'   # a local variable / pattern binding
            if turbofish and kind == 'val' and not called:
                base += turbofish
        return self.postfix_on(base, kind, toks[i:])

    def struct_literal_ahead(self, toks, i):
        inner = toks[i + 1:group_end(toks, i) - 1]
        if not inner:
            return True
        parts = split_toks(inner, ',')
        return all(p and (len(p) == 1 or p[1] == ':' or p[0] == '..') for p in parts if p)

    def struct_literal(self, path, inner):
        fields = []
        for p in split_toks(inner, ','):
            if not p:
                continue
            if p[0] == '..':
                fields.append('..' + self.expr(p[1:])[0])
            elif len(p) >= 2 and p[1] == ':':
                fields.append(f'{p[0]}: {self.expr(p[2:])[0]}')
            else:
                fields.append(f'{p[0]}: {self.expr(p)[0]}')
        return f'{self.resolve(path)} {{ {", ".join(fields)} }}'

    def macro(self, path, inner):
        if path == 'stringify':
            return '"' + join(inner) + '"', 'val'
        if path in self.f['defs']:
            exp = expand_macro(self.f['defs'], path, inner)
            if exp is None:
                raise Fail('macro ' + path)
            return self.expr(exp)
        name = path.split('::')[-1]
        if name in ('format', 'panic', 'assert', 'assert_eq', 'debug_assert', 'debug_assert_eq', 'unreachable', 'vec',
                    'matches', 'write', 'writeln', 'assert_ne', 'todo', 'unimplemented', 'concat', 'eprintln', 'println'):
            parts = [p for p in split_toks(inner, ',') if p]
            out = []
            for n, p in enumerate(parts):
                if name == 'matches' and n == 1:
                    out.append(self.resolve_types(p))
                elif n == 0 and name in ('format', 'panic', 'assert', 'write', 'writeln', 'eprintln', 'println', 'unreachable', 'todo', 'unimplemented') and p[0].startswith(('"', 'b"')):
                    out.append(join(p))
                elif len(p) >= 2 and p[1] == '=' and re.match(r'^[a-z_][a-z0-9_]*$', p[0]):
                    out.append(f'{p[0]} = {self.expr(p[2:])[0]}')
                else:
                    out.append(self.expr(p)[0])
            if parts and parts[0] and parts[0][0].startswith(('"', 'b"')) and name in ('format', 'panic', 'assert', 'write', 'writeln', 'eprintln', 'println', 'unreachable', 'todo', 'unimplemented'):
                given = {p[0] for p in parts[1:] if len(p) >= 2 and p[1] == '='}
                lit = parts[0][0]
                for cap in dict.fromkeys(re.findall(r'(?<!\{)\{([A-Za-z_][A-Za-z0-9_]*)(?::[^}]*)?\}', lit)):
                    if cap in given:
                        continue
                    if cap in self.env and self.env[cap][1] == 'val' and self.env[cap][0] != cap:
                        out.append(f'{cap} = {self.env[cap][0]}')
                    elif cap in self.env and self.env[cap][1] != 'val':
                        raise Fail('format capture of ' + str(self.env[cap][1]))
                    elif cap[0].isupper() or cap in self.f['consts']:
                        out.append(f'{cap} = {self.expr([cap])[0]}')
            return f'{self.resolve(path) if "::" in path else name}!({", ".join(out)})', 'val'
        raise Fail('macro ' + path)

    def postfix_on(self, base, kind, toks):
        i = 0
        while i < len(toks):
            t = toks[i]
            if t == '?':
                if kind == 'noq':
                    kind = 'val'
                elif kind == 'val':
                    if self.closure:
                        base = base + '?'
                    else:
                        if self.helper and not (self.helper[-1]['tail'] or self.helper[-1]['q']):
                            raise Fail('`?` inside a helper whose error is not propagated')
                        base = f'match {base} {{ Ok(v__) => v__, Err(err__) => return fail(e__, err__.into()) }}'
                elif kind == 'flinfo' or isinstance(kind, tuple):
                    pass
                else:
                    raise Fail(f'? on {kind}')
                i += 1
            elif t == '.' and i + 1 < len(toks):
                m = toks[i + 1]
                if m == 'await':
                    raise Fail('await')
                if i + 2 < len(toks) and toks[i + 2] == '(':
                    e = group_end(toks, i + 2)
                    args = split_args(toks[i + 3:e - 1])
                    base, kind = self.method(base, kind, m, args)
                    i = e
                elif i + 2 < len(toks) and toks[i + 2] == '::' and i + 3 < len(toks) and toks[i + 3] == '<':
                    ae = group_end_angle(toks, i + 3)
                    e = group_end(toks, ae) if ae < len(toks) and toks[ae] == '(' else ae
                    args = [a for a in split_toks(toks[ae + 1:e - 1], ',') if a] if e > ae else []
                    base, kind = self.method(base, kind, m + '::' + join(toks[i + 3:ae]), args)
                    i = e
                else:
                    base, kind = self.field(base, kind, m)
                    i += 2
            elif t == '(' and kind in ('val', 'noq'):
                e = group_end(toks, i)
                args = split_args(toks[i + 1:e - 1])
                base = base + '(' + ', '.join(self.expr(a)[0] for a in args) + ')'
                kind = 'val'
                i = e
            elif t == '[' and kind in ('val', 'noq'):
                e = group_end(toks, i)
                base = base + '[' + self.expr(toks[i + 1:e - 1])[0] + ']'
                i = e
            elif t == '[' and kind == 'args':
                e = group_end(toks, i)
                base, kind = f'a{int(toks[i + 1])}__', ('nd', int(toks[i + 1]))
                i = e
            elif t == 'as':
                return f'({base} as {self.resolve_types(toks[i + 1:])})', 'val'
            else:
                raise Fail('postfix ' + t + ' after ' + str(kind))
        return base, kind

    # -- calls
    def call(self, path, args, tail, tf=''):
        name = path.split('::')[-1]
        if path in self.env and self.env[path][1] == 'val':
            return f'({self.env[path][0]})({", ".join(self.expr(a)[0] for a in args)})', 'val'
        # fn_expr readers → leading type arguments
        if name == 'get_fn_expr_argtype' or path.endswith('get_fn_expr_argtype::call'):
            if len(args) != 2:
                raise Fail('get_fn_expr_argtype arity')
            fl, k = self.expr(args[0])
            if k != 'flinfo':
                raise Fail('get_fn_expr_argtype without flinfo')
            n = self.expr(args[1])[0]
            m = re.match(r'^\(*(\d+)', n)
            if not m:
                raise Fail('get_fn_expr_argtype index is not a literal')
            n = int(m.group(1))
            self.tyargs.add(n)
            return f't{n}__', 'val'
        if name == 'get_fn_expr_rettype' or path.endswith('get_fn_expr_rettype::call'):
            fl, k = self.expr(args[0])
            if k != 'flinfo':
                raise Fail('get_fn_expr_rettype without flinfo')
            self.tyargs.add(-1)
            return 'tr__', 'val'
        if name in VARATT_OF_PAYLOAD and len(args) == 1 and len(args[0]) == 1 and args[0][0] in self.env:
            k = self.env[args[0][0]][1]
            if isinstance(k, tuple) and k[0] == 'hdr':
                return VARATT_OF_PAYLOAD[name], 'val'
        if name in ('get_call_result_type', 'get_fn_expr_variadic', 'get_fn_expr_arg_stable', 'get_call_expr_argtype'):
            raise Ctx('FmgrInfo state across calls (' + name + ')')
        if name in SCRATCH_HELPERS and len(args) == 1 and args[0] and args[0][0] in ('|', 'move'):
            cl = args[0]
            j = cl.index('|', 1)
            pn = [x for x in cl[1:j] if x != ',']
            if len(pn) != 1:
                raise Fail('scratch helper closure params')
            body = cl[j + 1:]
            if body[0] == '{' and group_end(body, 0) == len(body):
                body = body[1:-1]
            self.uses_mcx = True
            saved = dict(self.env)
            self.env[pn[0]] = ('mcx__', 'mcx')
            try:
                if tail:
                    return '{ ' + self.block_ret(body) + ' }', 'ret'
                return '{ ' + self.block_value(body) + ' }', 'val'
            finally:
                self.env = saved
        # a local helper: inlined when it takes the frame, returns a Datum, or is not reachable by path
        if path in self.f['fns'] and '::' not in path:
            sig, _, vis = self.f['fns'][path]
            if 'Fcinfo' in sig or 'FunctionCallInfoBaseData' in sig or 'fcinfo' in sig or 'FmgrInfo' in sig or self.datum_returning(sig) or vis != 'pub':
                return self.inline_fn(path, args, tail, tf)
        other = self.find_helper(path)
        if other is not None:
            saved_f = self.f
            self.f = other
            try:
                return self.inline_fn(name, args, tail)
            finally:
                self.f = saved_f
        # D3594: the name wrappers read a name argument as name_block_from_ptr(fcinfo.arg_ptr(i)) (pgrust main
        # a498bdc114b: the bytes before the NUL, at most NAMEDATALEN; a btree name_ops key is a cstring). On a view
        # argument that is the view's name64() image, the same NUL-bounded bytes the arg_name form maps to.
        if path.split('::')[-1] == 'name_block_from_ptr' and len(args) == 1:
            v, k = self.expr(args[0])
            if isinstance(k, tuple) and k[0] == 'hdr' and self.cls(k[1]) == 'view':
                i = k[1]
                if f'let name{i}__ = a{i}__.name64();' not in self.prelude:
                    self.prelude.append(f'let name{i}__ = a{i}__.name64();')
                return f'name{i}__', 'val'
        p = path if path in ('Some', 'Ok', 'Err') else self.resolve(path)
        if p in ('Ok',) and len(args) == 1:
            v, k = self.expr(args[0])
            if k == 'null':
                return v, 'null'
            v, k = self.as_value(v, k)
            if k not in ('val', 'noq'):
                raise Fail(f'Ok({k})')
            return f'Ok({v})', 'val'
        out = []
        for a in args:
            v, k = self.expr(a)
            v, k = self.as_value(v, k)
            if k == 'fcinfo':
                raise Fail('fcinfo passed to ' + path)
            if k == 'flinfo':
                raise Ctx('FmgrInfo state across calls (passed to ' + name + ')')
            if k == 'mcx':
                self.uses_mcx = True
            if k not in ('val', 'noq', 'mcx', 'coll'):
                raise Fail(f'{k} passed to {path}')
            out.append(v)
        crate = p.split('::')[0]
        if crate in CATALOG_CRATES:
            raise Ctx('catalog (' + p + ')')
        return f'{p}{tf}({", ".join(out)})', 'val'

    def find_helper(self, path):
        """A fn in another file of the workspace that takes the frame -> that file's record."""
        full = self.resolve_inner(path)
        segs = full.split('::')
        if len(segs) < 2:
            return None
        crate, name = segs[0], segs[-1]
        module = '::' + '::'.join(segs[1:-1]) if len(segs) > 2 else ''
        for of in FILES.values():
            if of['crate'] == crate and of['module'] == module and name in of['fns'] and name in of['locals']:
                sig = of['fns'][name][0]
                if 'Fcinfo' in sig or 'FunctionCallInfoBaseData' in sig or 'fcinfo' in sig or self.datum_returning(sig):
                    return of
                return None
        return None

    def extra_type(self, tn):
        """The `fn_extra` type of this shim: a per-shim thread-local static holds it (a private
        single-field newtype of the wrapper's file is held by its field type)."""
        inner = self.f['newtypes'].get(tn)
        rt = self.resolve_types(inner) if inner is not None else self.resolve_types(tokenize(tn))
        if self.extra is not None and self.extra != rt:
            raise Fail('two fn_extra types')
        self.extra = rt
        return rt

    def extra_ptr(self):
        self.uses_extra = True
        return 'EXTRA__.with(|c__| c__.get())'

    def strip_newtype(self, toks):
        if len(toks) >= 3 and toks[0] in self.f['newtypes'] and toks[1] == '(' and group_end(toks, 1) == len(toks):
            return toks[2:-1]
        return toks

    def as_value(self, v, k):
        """A transport-kind value used as a plain value: a Datum for `fcinfo.arg(i)`, a
        PackedVarlena over a scratch image for a payload view (one copy)."""
        if isinstance(k, tuple) and k[0] == 'ndv':
            i = k[1]
            c = self.cls(i)
            if c in ('byval', 'datum'):
                return f'::datum::Datum::from_i64(a{i}__)', 'val'
            if c in ('fixed', 'img') or (c == 'byref' and self.sqlargs[i] == 'interval'):
                return f'::datum::Datum::from_usize(a{i}__.ptr as usize)', 'val'
            self.uses_mcx = True
            return f'::datum::Datum::from_usize(crate::scratch_image(mcx__, a{i}__.bytes()).as_ptr() as usize)', 'val'
        if isinstance(k, tuple) and k[0] == 'packed':
            i = k[1]
            self.uses_mcx = True
            return f'(unsafe {{ ::fmgr::PackedVarlena::from_ptr(crate::scratch_image(mcx__, a{i}__.bytes()).as_ptr()) }})', 'val'
        return v, k

    def params_of(self, sig):
        """(name, is_fcinfo_like) for each parameter of a fn signature token list."""
        i = 0
        while sig[i] != '(':
            i = group_end_angle(sig, i) if sig[i] == '<' else i + 1
        e = group_end(sig, i)
        out = []
        parts, cur, depth = [], [], 0
        for tk in sig[i + 1:e - 1]:
            if tk in OPEN or tk == '<':
                depth += 1
            elif tk == '<<':
                depth += 2
            elif tk in CLOSE or tk == '>':
                depth -= 1
            elif tk == '>>':
                depth -= 2
            if tk == ',' and depth == 0:
                parts.append(cur)
                cur = []
            else:
                cur.append(tk)
        parts.append(cur)
        for p in parts:
            if not p:
                continue
            k = 0
            if p[0] == 'mut':
                k = 1
            if p[k] in ('&', 'self') or (k + 1 < len(p) and p[k + 1] != ':'):
                if p[k] == 'self' or (p[k] == '&' and 'self' in p[:k + 3]):
                    raise Fail('method helper')
                raise Fail('helper parameter pattern: ' + join(p))
            out.append(p[k])
        return out, sig[e:]

    def datum_returning(self, sig):
        s = join(sig)
        return bool(re.search(r'->\s*(::)?(datum::)?Datum\s*$', s)) or bool(re.search(r'->\s*(::)?(types_error::)?PgResult<\s*(::)?(datum::)?Datum\s*>\s*$', s))

    def inline_fn(self, name, args, tail, tf=''):
        if self.depth > 6:
            raise Fail('inlining depth')
        sig, body, vis = self.f['fns'][name]
        params, ret = self.params_of(sig)
        generics = []
        if sig and sig[0] == '<':
            for g in split_toks(sig[1:group_end_angle(sig, 0) - 1], ','):
                if g and g[0] == 'const':
                    generics.append(g[1])
        if generics:
            vals = split_toks(tokenize(tf[3:-1]), ',') if tf.startswith('::<') else []
            if len(vals) != len(generics):
                raise Fail('const generic ' + generics[0] + ' without a turbofish')
            body = list(body)
            for gname, val in zip(generics, vals):
                body = [('{ ' + join(val) + ' }') if t == gname else t for t in body]
                ret = [('{ ' + join(val) + ' }') if t == gname else t for t in ret]
        if len(params) != len(args):
            raise Fail(f'helper {name} arity')
        plain_datum = tail and bool(re.search(r'->\s*(::)?(datum::)?Datum\s*$', join(sig)))
        rty = ''
        if not tail and ret and ret[0] == '->':
            rt = ret[1:]
            while rt and rt[-1] in ('where',):
                rt = rt[:-1]
            if 'where' in rt:
                rt = rt[:rt.index('where')]
            if 'impl' not in rt and 'dyn' not in rt and not any(t == 'T' for t in rt):
                rty = self.resolve_types(rt)
        return self.inline_body(name, params, args, body, tail, has_return=self.has_return(body), ok_mode=plain_datum, rty=rty)

    def inline_closure(self, closure, args):
        i = 0
        if closure[0] == 'move':
            i = 1
        if closure[i] == '||':
            params, j = [], i + 1
        else:
            j = closure.index('|', i + 1)
            params = split_toks(closure[i + 1:j], ',')
            j += 1
        names = []
        for p in params:
            if ':' in p:
                p = p[:p.index(':')]
            if len(p) != 1 or p[0] == '_' and False:
                if len(p) == 1 and p[0] == '_':
                    names.append('_')
                    continue
                raise Fail('closure parameter pattern')
            names.append(p[0])
        body = closure[j:]
        if body and body[0] == '{' and group_end(body, 0) == len(body):
            body = body[1:-1]
        elif body and body[0] == '{':
            raise Fail('closure body shape')
        if len(names) != len(args):
            raise Fail('closure arity')
        return self.inline_body('closure', names, args, body, tail=False, has_return=self.has_return(body), value_only=True)[0]

    def has_return(self, body):
        depth = 0
        for t in body:
            if t == '|':
                pass
            if t == 'return':
                return True
        return False

    def inline_body(self, name, params, args, body, tail, has_return, value_only=False, ok_mode=False, rty=''):
        """Bind the helper's parameters to the call's arguments and translate its body as a block."""
        saved_env, saved_helper = dict(self.env), self.helper
        lets = []
        newenv = {}
        for p, a in zip(params, args):
            v, k = self.expr(a)
            if k in ('val', 'noq'):
                if len(a) == 1 or v.startswith('a') and v.endswith('__') or re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', v) or re.match(r'^(Some\([A-Za-z_][A-Za-z0-9_]*\)|Some\(-?\d+[a-z0-9]*\)|-?\d+[a-z0-9]*|true|false|None)$', v) or re.match(r'^[\d()+\-* ]+(usize)?$', v.replace('usize', '')):
                    newenv[p] = (v, 'val')
                else:
                    nm = self.gensym(p.lstrip('_') or 'p')
                    if v.startswith('|') and 'Ok(' in v and '->' not in v.split('{', 1)[0]:
                        # a closure literal whose `Ok(..)` leaves the error type open: the wrappers' helpers take `PgResult`
                        v = v.split('|', 2)
                        v = '|' + v[1] + '| -> types_error::PgResult<_> ' + v[2].lstrip()
                    lets.append(f'let {nm} = {v};')
                    newenv[p] = (nm, 'val')
            else:
                if k == 'mcx':
                    self.uses_mcx = True
                newenv[p] = (v, k)
        # names the helper body binds itself shadow the outer scope; unknown names pass verbatim
        self.env = {k: v for k, v in saved_env.items() if v[1] in ('mcx', 'coll')}
        self.env.update(newenv)
        self.helper = saved_helper + [dict(tail=tail, q=self.questioned)]
        self.depth += 1
        try:
            if tail:
                text = self.block_ret(body, ok=ok_mode)
                out = '{ ' + ' '.join(lets) + ' ' + text + ' }', 'ret'
            elif not self.closure and rty and '?' in body and not self.questioned and not tail and 'return_null' not in body:
                # the helper's own `?` returns its Result to the caller, who inspects it
                self.closure += 1
                try:
                    text = self.block_value(body)
                finally:
                    self.closure -= 1
                out = '{ ' + ' '.join(lets) + ' (|| -> ' + rty + ' { ' + text + ' })() }', 'val'
            elif has_return and re.search(r'\breturn\s+(?!Err\b)', join(body)):
                # a helper that returns a value early: an immediately-invoked closure keeps its `return`s
                if 'return_null' in body:
                    raise Fail(f'helper {name} returns null early')
                self.closure += 1
                try:
                    text = self.block_value(body)
                finally:
                    self.closure -= 1
                out = '{ ' + ' '.join(lets) + ' (|| ' + (f'-> {rty} ' if rty else '') + '{ ' + text + ' })() }', 'val'
            else:
                text = self.block_value(body)
                if rty:
                    out = '{ ' + ' '.join(lets) + ' let out__: ' + rty + ' = { ' + text + ' }; out__ }', 'val'
                else:
                    out = '{ ' + ' '.join(lets) + ' ' + text + ' }', 'val'
        finally:
            self.env = saved_env
            self.helper = saved_helper
            self.depth -= 1
        if out[1] == 'val' and self.lit_bool(out[0]) is not None:
            return self.lit_bool(out[0]), 'val'
        return out

    questioned = False

    # -- methods and fields
    def method(self, base, kind, m, args):
        mname = m.split('::')[0]
        if kind == 'fcinfo':
            if mname in CONTEXT_METHODS:
                raise Ctx(CONTEXT_METHODS[mname])
            if mname == 'args_n':
                return 'args', 'args'
            if mname.startswith('arg_') and mname[4:] in ('i8', 'i16', 'i32', 'i64', 'u32', 'u64', 'f32', 'f64', 'bool', 'char', 'oid'):
                return self.arg_byval(self.arg_index(args), GETTER['as_' + mname[4:]]), 'val'
            if mname == 'arg':
                i = self.arg_index(args)
                return f'a{i}__', ('ndv', i)
            if mname == 'argisnull':
                self.arg_index(args)
                return 'false', 'val'
            if mname == 'has_null_args':
                return 'false', 'val'
            if mname == 'nargs':
                return str(len(self.sqlargs)), 'val'
            if mname in ('arg_varlena_packed', 'arg', 'arg_ptr', 'arg_fixed', 'arg_cstring', 'arg_uuid', 'arg_tid', 'arg_name', 'arg_interval', 'arg_varlena_raw') and self.arg_index(args) >= len(self.sqlargs):
                ty = {'arg_varlena_packed': "::fmgr::PackedVarlena<'_>", 'arg': '::datum::Datum', 'arg_ptr': '*const u8', 'arg_fixed': '&[u8]', 'arg_varlena_raw': '&[u8]',
                      'arg_cstring': '&core::ffi::CStr', 'arg_uuid': '&[u8; 16]', 'arg_tid': '&[u8; 6]', 'arg_name': '&[u8; 64]', 'arg_interval': '&[u8; 16]'}[mname]
                return f'{{ let dead__: {ty} = unreachable!(); dead__ }}', 'noq' if mname == 'arg_varlena_packed' else 'val'
            if mname == 'arg_varlena_packed':
                i = self.arg_index(args)
                c = self.cls(i)
                if c in ('view', 'byref'):
                    return f'a{i}__', ('packed', i)
                if c in ('img', 'datum'):
                    return f'(unsafe {{ ::fmgr::PackedVarlena::from_ptr({self.arg_ptr(i)}) }})', 'noq'
                raise Fail(f'arg_varlena_packed on {self.sqlargs[i]}')
            if mname == 'arg_varlena_raw':
                i = self.arg_index(args)
                if i >= len(self.sqlargs):
                    return '(unreachable!())', 'val'
                if self.cls(i) == 'img':
                    return f'a{i}__.bytes()', 'val'
                if self.cls(i) == 'datum':
                    return f'crate::shims::raw::varlena_image(a{i}__)', 'val'
                if self.cls(i) in ('view', 'byref'):
                    self.uses_mcx = True
                    return f'crate::scratch_image(mcx__, a{i}__.bytes())', 'val'
                raise Fail(f'arg_varlena_raw on {self.sqlargs[i]}')
            if mname == 'arg_varlena_is_extended':
                i = self.arg_index(args)
                if i >= len(self.sqlargs):
                    return 'false', 'val'
                if self.cls(i) in ('view', 'byref'):
                    return 'false', 'val'
                raise Fail(f'arg_varlena_is_extended on {self.sqlargs[i]}')
            if mname == 'arg_ptr':
                i = self.arg_index(args)
                if i < len(self.sqlargs) and self.cls(i) in ('view', 'byref') and not (self.cls(i) == 'byref' and self.sqlargs[i] == 'interval'):
                    return f'a{i}__.ptr', ('hdr', i)
                return self.arg_ptr(i), 'val'
            if mname == 'arg_fixed':
                i = self.arg_index(args)
                if self.cls(i) != 'fixed':
                    raise Fail(f'arg_fixed on {self.sqlargs[i]}')
                return f'a{i}__.bytes()', 'val'
            if mname in ('arg_uuid', 'arg_tid', 'arg_name', 'arg_interval'):
                i = self.arg_index(args)
                n = {'arg_uuid': 16, 'arg_tid': 6, 'arg_name': 64, 'arg_interval': 16}[mname]
                if self.cls(i) == 'view' and mname == 'arg_name':
                    if f'let name{i}__ = a{i}__.name64();' not in self.prelude:
                        self.prelude.append(f'let name{i}__ = a{i}__.name64();')
                    return f'&name{i}__', 'val'
                if self.cls(i) == 'byref' and mname == 'arg_interval':
                    return f'&a{i}__.interval_bytes()', 'val'
                if self.cls(i) != 'fixed':
                    raise Fail(f'{mname} on {self.sqlargs[i]}')
                return f'a{i}__.fixed::<{n}>()', 'val'
            if mname == 'arg_cstring':
                i = self.arg_index(args)
                if self.cls(i) != 'view':
                    raise Fail(f'arg_cstring on {self.sqlargs[i]}')
                return f'a{i}__', ('cstr', i)
            if mname == 'get_collation':
                self.uses_coll = True
                return 'coll__', 'coll'
            if mname in ('result_mcx', 'result_mcx_detached'):
                self.uses_mcx = True
                return 'mcx__', 'mcx'
            if mname == 'return_null':
                return 'null', 'null'
            if mname in ('set_arg', 'set_arg_null', 'rearm', 'init', 'set_result_mcx'):
                raise Fail('mutates the frame (' + mname + ')')
            if mname == 'arg_stringinfo':
                raise Fail('recv function')
            raise Fail('fcinfo.' + mname)
        if kind == 'flinfo':
            if mname in FLINFO_PASS:
                return base, 'flinfo'
            if mname == 'has_fn_extra':
                return f'(*{self.extra_ptr()}).is_some()', 'val'
            if mname == 'set_fn_extra':
                if len(args) != 1:
                    raise Fail('set_fn_extra arity')
                a0 = args[0]
                if len(a0) >= 3 and a0[0] in self.f['newtypes'] and a0[1] == '(' and group_end(a0, 1) == len(a0):
                    self.extra_type(a0[0])
                    a0 = a0[2:-1]
                v = self.expr(a0)[0]
                return f'{{ *{self.extra_ptr()} = Some({v}); }}', 'val'
            if mname in ('fn_extra_mut', 'fn_extra_ref'):
                tn = m.split('::<', 1)[1].rstrip('>').strip() if '::<' in m else None
                if tn is None:
                    raise Fail(mname + ' without a type')
                self.extra_type(tn)
                acc = 'as_mut' if mname == 'fn_extra_mut' else 'as_ref'
                return f'(*{self.extra_ptr()}).{acc}()', ('extraopt', tn)
            raise Ctx('FmgrInfo state across calls (flinfo.' + mname + ')')
        if isinstance(kind, tuple) and kind[0] == 'extraopt':
            if mname in ('unwrap', 'expect'):
                return f'{base}.{mname}({", ".join(self.expr(a)[0] for a in args)})', ('extraval', kind[1])
            return f'{base}.{m}({", ".join(self.expr(a)[0] for a in args)})', 'val'
        if isinstance(kind, tuple) and kind[0] == 'extraval':
            return f'{base}.{m}({", ".join(self.expr(a)[0] for a in args)})', 'val'
        if kind == 'noq' and mname in ('expect', 'unwrap', 'unwrap_or_else', 'unwrap_or'):
            return base, 'val'
        if kind == 'noq' and mname == 'ok':
            return f'Some({base})', 'val'
        if kind == 'mcx':
            self.uses_mcx = True
            return f'mcx__.{m}({", ".join(self.expr(a)[0] for a in args)})', 'val'
        if kind == 'coll':
            return f'coll__.{m}({", ".join(self.expr(a)[0] for a in args)})', 'val'
        if kind == 'null':
            raise Fail('method on a null result')
        if isinstance(kind, tuple):
            k0, i = kind[0], kind[1] if len(kind) > 1 else None
            if k0 == 'nd':
                raise Fail('nd.' + mname)
            if k0 == 'ndv':
                if mname in GETTER:
                    if GETTER[mname] == 'usize':
                        c = self.cls(i)
                        if c in ('fixed', 'img'):
                            return f'(a{i}__.ptr as usize)', 'val'
                        if c == 'datum':
                            return f'(a{i}__ as usize)', 'val'
                        raise Fail(f'as_usize on {self.sqlargs[i]} (a payload view)')
                    return self.arg_byval(i, GETTER[mname]), 'val'
                if mname in ('is_null',):
                    return 'false', 'val'
                if self.cls(i) == 'datum':
                    return f'::datum::Datum::from_i64(a{i}__).{m}({", ".join(self.expr(a)[0] for a in args)})', 'val'
                raise Fail('datum.' + mname)
            if k0 == 'packed':
                if mname == 'data':
                    return f'a{i}__.bytes()', 'val'
                if mname == 'is_short':
                    return 'false', 'val'
                if mname == 'data_expanded':
                    self.uses_mcx = True
                    return f'a{i}__.bytes()', 'noq'
                if mname == 'size':
                    return f'(a{i}__.len as usize + 4)', 'val'
                v, _ = self.as_value(base, kind)
                return f'{v}.{m}({", ".join(self.expr(a)[0] for a in args)})', 'val'
            if k0 == 'cstr':
                if mname == 'to_bytes':
                    return f'a{i}__.bytes()', 'val'
                if mname == 'to_str':
                    return f'core::str::from_utf8(a{i}__.bytes())', 'val'
                if mname == 'to_string_lossy':
                    return f'String::from_utf8_lossy(a{i}__.bytes())', 'val'
                if mname == 'to_bytes_with_nul':
                    self.uses_mcx = True
                    return f'crate::scratch_cstr(mcx__, a{i}__.bytes())', 'val'
                raise Fail('cstr.' + mname)
            if k0 == 'tl':
                if mname == 'with' and len(args) == 1:
                    return self.thread_local(kind[1], args[0])
                raise Fail('thread-local.' + mname)
        if mname in ('is_some', 'is_none') and not args and kind == 'val':
            if base == 'None':
                return ('false' if mname == 'is_some' else 'true'), 'val'
            if re.match(r'^Some\(', base):
                return ('true' if mname == 'is_some' else 'false'), 'val'
        out = []
        for a in args:
            v, k = self.expr(a)
            v, k = self.as_value(v, k)
            if k == 'mcx':
                self.uses_mcx = True
            elif k == 'fcinfo':
                raise Fail('fcinfo passed to method ' + mname)
            elif k == 'flinfo':
                raise Ctx('FmgrInfo state across calls (passed to ' + mname + ')')
            elif k not in ('val', 'noq', 'coll'):
                raise Fail(f'{k} passed to method {mname}')
            out.append(v)
        return f'{base}.{m}({", ".join(out)})', 'val'

    def field(self, base, kind, m):
        if kind == 'fcinfo':
            if m == 'fncollation':
                self.uses_coll = True
                return 'coll__', 'coll'
            if m == 'args':
                return 'args', 'args'
            if m in CONTEXT_METHODS:
                raise Ctx(CONTEXT_METHODS[m])
            if m == 'isnull':
                return 'isnull', 'isnull'
            raise Fail('fcinfo.' + m)
        if kind == 'flinfo':
            if m == 'fn_oid':
                raise Ctx('FmgrInfo state across calls (fn_oid)')
            raise Ctx('FmgrInfo state across calls (flinfo.' + m + ')')
        if isinstance(kind, tuple) and kind[0] == 'extraval':
            if m == '0' and kind[1] in self.f['newtypes']:
                return f'(*{base})', 'val'
            return f'{base}.{m}', 'val'
        if isinstance(kind, tuple) and kind[0] == 'nd':
            if m == 'value':
                return f'a{kind[1]}__', ('ndv', kind[1])
            if m == 'isnull':
                return 'false', 'val'
            raise Fail('nd.' + m)
        if isinstance(kind, tuple) and kind[0] == 'ndv':
            raise Fail('datum.' + m)
        if kind not in ('val', 'noq'):
            raise Fail(f'field {m} on {kind}')
        return f'{base}.{m}', 'val'

    def thread_local(self, name, closure):
        """`SCRATCH.with(|c| BODY)` over an `UnsafeCell<[u8; N]>` static: a shim-local buffer instead."""
        ty = self.f['tls'][name]
        s = join(ty)
        m = re.match(r'^(?:core::cell::|std::cell::)?UnsafeCell<\[u8;(.*)\]>$', s.replace(' ', ''))
        if not m:
            raise Fail('thread-local ' + name + ' of type ' + s)
        n = self.expr(tokenize(m.group(1)))[0]
        i = 0
        if closure[0] == 'move':
            i = 1
        j = closure.index('|', i + 1)
        params = split_toks(closure[i + 1:j], ',')
        if len(params) != 1 or len(params[0]) != 1:
            raise Fail('thread-local closure params')
        buf = self.gensym('scratch')
        self.prelude.append(f'let {buf} = core::cell::UnsafeCell::new([0u8; {n}]);')
        body = closure[j + 1:]
        if body[0] == '{' and group_end(body, 0) == len(body):
            body = body[1:-1]
        saved = dict(self.env)
        self.env[params[0][0]] = (f'(&{buf})', 'val')
        try:
            if self.want_ret:
                return '{ ' + self.block_ret(body) + ' }', 'ret'
            return '{ ' + self.block_value(body) + ' }', 'val'
        finally:
            self.env = saved

    want_ret = False

    # -- statements
    def stmts(self, toks):
        """Split a block into statements, keeping `if`/`match`/`for`/`while`/`loop` blocks whole."""
        out, i = [], 0
        while i < len(toks):
            t = toks[i]
            if t in ('if', 'match', 'for', 'while', 'loop', 'unsafe') or t == '{':
                j = i
                if t == 'if':
                    j = self.find_brace(toks, i + 1)
                    j = group_end(toks, j)
                    while j < len(toks) and toks[j] == 'else':
                        j = self.find_brace(toks, j + 1)
                        j = group_end(toks, j)
                elif t in ('match', 'for', 'while'):
                    j = self.find_brace(toks, i + 1)
                    j = group_end(toks, j)
                elif t in ('loop', 'unsafe'):
                    j = group_end(toks, i + 1)
                else:
                    j = group_end(toks, i)
                # a block followed by a method / operator continues the expression
                if j < len(toks) and toks[j] in ('.', '?', 'as'):
                    k = j
                    while k < len(toks) and toks[k] != ';':
                        k = group_end(toks, k) if toks[k] in OPEN else k + 1
                    out.append((toks[i:k], k < len(toks)))
                    i = k + 1
                    continue
                out.append((toks[i:j], j < len(toks) and toks[j] == ';'))
                i = j + 1 if j < len(toks) and toks[j] == ';' else j
                continue
            k = i
            while k < len(toks) and toks[k] != ';':
                k = group_end(toks, k) if toks[k] in OPEN else k + 1
            out.append((toks[i:k], k < len(toks)))
            i = k + 1
        return out

    def diverges(self, s):
        """An `if` statement that folds to a block whose last statement is a `return`."""
        if not s or s[0] != 'if':
            return False
        saved_env, saved_mcx, saved_coll = dict(self.env), self.uses_mcx, self.uses_coll
        try:
            folded = self.fold_if(s)
        except (Fail, Ctx):
            folded = None
        self.env, self.uses_mcx, self.uses_coll = saved_env, saved_mcx, saved_coll
        if folded is None:
            return False
        if folded[0] == 'if':
            return self.diverges(folded)
        ss = [x for x in self.stmts(folded[1:-1]) if x[0]]
        return bool(ss) and ss[-1][0][0] == 'return'

    def block_value(self, toks):
        """A block whose value is an ordinary Rust value."""
        lines = []
        ss = self.stmts(toks)
        for n, (s, semi) in enumerate(ss):
            if not s:
                continue
            last = n == len(ss) - 1
            text = self.stmt(s, semi, last and not semi, ret=False)
            if text:
                lines.append(text)
            if not last and self.diverges(s):
                break
        return ' '.join(lines)

    def block_ret(self, toks, ok=False):
        """A block in result position: its tail (and every `return`) is finished into a Ret; in
        `ok` mode the tail is a Datum (the block sits inside `Ok(..)` or a Datum-returning helper)."""
        lines = []
        ss = self.stmts(toks)
        if not ss:
            raise Fail('empty block in result position')
        for n, (s, semi) in enumerate(ss):
            if not s:
                continue
            last = n == len(ss) - 1
            if last and not semi:
                lines.append(self.ret_ok(s) if ok else self.ret_text(s))
            elif last and semi and s[0] == 'return':
                lines.append(self.stmt(s, semi, False, ret=True))
            elif last:
                raise Fail('no tail expression')
            else:
                text = self.stmt(s, semi, False, ret=True)
                if text:
                    lines.append(text)
                if self.diverges(s):
                    break
        return ' '.join(lines)

    def stmt(self, s, semi, is_tail, ret):
        if s[0] == 'let':
            return self.let(s)
        if s[0] == 'const' and len(s) > 1 and s[1] == '{':
            return ''
        if s[0] == 'use':
            return 'use ' + self.resolve_types(s[1:]) + ';'
        if s[0] == 'return':
            if self.closure:
                return 'return ' + (self.expr(s[1:])[0] if len(s) > 1 else '') + ';'
            return 'return ' + self.ret_text(s[1:] if len(s) > 1 else ['(', ')']) + ';'
        if s[0] == 'fcinfo' and len(s) >= 4 and s[1] == '.' and s[2] == 'isnull' and s[3] == '=':
            return ''
        if s[0] == 'if' and not is_tail:
            return self.if_stmt(s) + ('' if not semi else ';')
        if s[0] == 'match' and not is_tail:
            return self.match_expr(s, False) + ('' if not semi else ';')
        v, k = self.expr(s)
        if k not in ('val', 'noq', 'flinfo'):
            raise Fail(f'statement of kind {k}')
        if k == 'flinfo':
            return ''
        return v + (';' if semi else '')

    def if_stmt(self, s):
        folded = self.fold_if(s)
        if folded is not None:
            if folded[0] == 'if':
                return self.if_stmt(folded)
            return '{ ' + self.block_value(folded[1:-1]) + ' }'
        j = self.find_brace(s, 1)
        cond = self.cond(s[1:j])
        te = group_end(s, j)
        then = self.block_value(s[j + 1:te - 1])
        out = f'if {cond} {{ {then} }}'
        while te < len(s) and s[te] == 'else':
            if s[te + 1] == 'if':
                k = self.find_brace(s, te + 2)
                c2 = self.cond(s[te + 2:k])
                ke = group_end(s, k)
                out += f' else if {c2} {{ {self.block_value(s[k + 1:ke - 1])} }}'
                te = ke
            else:
                ee = group_end(s, te + 1)
                out += f' else {{ {self.block_value(s[te + 2:ee - 1])} }}'
                te = ee
        return out

    def let(self, s):
        # let [mut] PAT [: T] = EXPR [else { .. }]
        eq = None
        depth = 0
        for i, t in enumerate(s):
            if t in OPEN:
                depth += 1
            elif t in CLOSE:
                depth -= 1
            elif depth == 0 and t == '=' and (i + 1 >= len(s) or s[i + 1] not in ('=', '>')):
                eq = i
                break
        if eq is None:
            return 'let ' + self.resolve_types(s[1:]) + ';'
        pat, rhs = s[1:eq], s[eq + 1:]
        els = ''
        if 'else' in rhs and rhs[0] not in ('if', 'unsafe', 'match'):
            # `let PAT = EXPR else { .. }` — the else block never falls through; an `if` at depth 0
            # before the `else` makes it an if-expression instead
            depth = 0
            for i, t in enumerate(rhs):
                if t in OPEN:
                    depth += 1
                elif t in CLOSE:
                    depth -= 1
                elif depth == 0 and t == 'if':
                    break
                elif depth == 0 and t == 'else':
                    els_toks = rhs[i + 2:group_end(rhs, i + 1) - 1]
                    rhs = rhs[:i]
                    els = ' else { ' + self.block_value(els_toks) + ' }'
                    break
        mut = ''
        if pat and pat[0] == 'mut':
            mut, pat = 'mut ', pat[1:]
        ty = ''
        if ':' in pat and '::' not in pat[:pat.index(':') + 1]:
            ci = pat.index(':')
            ty = ': ' + self.resolve_types(pat[ci + 1:])
            pat = pat[:ci]
        if pat[0] == '[':
            names = [n[0] for n in split_toks(pat[1:-1], ',')]
            r = rhs[1:] if rhs[0] == '*' else rhs
            v, k = self.expr(r)
            if k != 'args':
                raise Fail('let [] = ' + join(rhs)[:40])
            for i, nm in enumerate(names):
                if nm != '_':
                    self.env[nm] = (f'a{i}__', ('nd', i))
            return ''
        r = rhs[2:group_end(rhs, 1) - 1] if rhs[0] == 'unsafe' and group_end(rhs, 1) == len(rhs) else rhs
        for _ in range(4):
            if r and r[0] == 'if':
                folded = self.fold_if(r)
                if folded is None or folded[0] == 'if' and folded is r:
                    break
                r = folded[1:-1] if folded[0] == '{' else folded
                continue
            if r and r[0] == 'match':
                folded = self.fold_match(r)
                if folded is None:
                    break
                r = folded
                continue
            break
        # `let (a, b) = { stmts; (x, y) }` (often under `unsafe`): the statements first, then the elements
        pre_lines = []
        if pat[0] == '(' and r and (r[0] == '{' and group_end(r, 0) == len(r) or r[0] == 'let'):
            inner_ss = self.stmts(r[1:-1] if r[0] == '{' else r)
            if inner_ss and not inner_ss[-1][1] and inner_ss[-1][0] and inner_ss[-1][0][0] == '(' and group_end(inner_ss[-1][0], 0) == len(inner_ss[-1][0]):
                for st, semi in inner_ss[:-1]:
                    if st:
                        txt = self.stmt(st, semi, False, ret=False)
                        if txt:
                            pre_lines.append(txt)
                r = inner_ss[-1][0]
        if pat[0] == '(' and r and r[0] == '(' and group_end(r, 0) == len(r):
            names = split_toks(pat[1:-1], ',')
            parts = split_toks(r[1:-1], ',')
            if len(parts) == len(names) and all(len(n) == 1 for n in names):
                out = list(pre_lines)
                for nm_, p_ in zip(names, parts):
                    pv, pk = self.expr(p_)
                    nm_ = nm_[0]
                    if pk in ('val', 'noq'):
                        o = nm_ + '_v__' if nm_.endswith('__') or nm_ in ('e', 'r', 'coll', 'mcx', 'args') else nm_
                        out.append(f'let {mut}{o} = {pv};')
                        self.env[nm_] = (pv if re.match(r'^-?\d+[a-z0-9]*$', pv) or pv in ('true', 'false', 'None') else o, 'val')
                    else:
                        if pk == 'mcx':
                            self.uses_mcx = True
                        self.env[nm_] = (pv, pk)
                return ' '.join(out)
        v, k = self.expr(rhs)
        if k == 'flinfo' and len(pat) == 4 and pat[0] == 'Some' and pat[1] == '(':
            # `let Some(fl) = flinfo else { no_flinfo(..) }`: the shim always has its FmgrInfo model
            self.env[pat[2]] = ('flinfo', 'flinfo')
            return ''
        if pat[0] == '(' and k in ('val', 'noq'):
            for n in self.pattern_names(pat):
                self.env[n] = (n, 'val')
            return f'let {mut}{self.resolve_types(pat)}{ty} = {v}{els};'
        if k == 'flinfo' and pat[0] != 'Some':
            raise Ctx('FmgrInfo state across calls (let over flinfo)')
        if pat[0] == '(':
            raise Fail('tuple let over ' + str(k))
        if len(pat) != 1:
            for n in self.pattern_names(pat):
                self.env[n] = (n, 'val')
            if k not in ('val', 'noq'):
                raise Fail('pattern let over ' + str(k))
            return f'let {mut}{self.resolve_types(pat)}{ty} = {v}{els};'
        nm = pat[0]
        if isinstance(k, tuple) and k[0] in ('extraval', 'extraopt'):
            k = 'val'
        if k in ('val', 'noq'):
            out = nm + '_v__' if nm.endswith('__') or nm in ('e', 'r', 'coll', 'mcx', 'args') else nm
            self.env[nm] = (out, 'val')
            return f'let {mut}{out}{ty} = {v}{els};'
        self.env[nm] = (v, k)
        return ''

    # -- the result
    def ret_text(self, toks):
        """An expression of type PgResult<Datum> in result position -> text of type Ret."""
        toks = list(toks)
        while toks and toks[0] == '(' and group_end(toks, 0) == len(toks):
            toks = toks[1:-1]
        if not toks:
            raise Fail('empty result')
        t = toks[0]
        if t == 'unsafe' and toks[1] == '{' and group_end(toks, 1) == len(toks):
            return self.ret_text(toks[2:-1])
        if t == '{' and group_end(toks, 0) == len(toks):
            return self.block_ret(toks[1:-1])
        if t == 'if':
            return self.if_expr(toks, True)
        if t == 'match':
            return self.match_expr(toks, True)
        if t == 'return':
            return self.ret_text(toks[1:])
        if t == 'Ok' and toks[1] == '(' and group_end(toks, 1) == len(toks):
            return self.ret_ok(toks[2:-1])
        if t == 'Err' and toks[1] == '(' and group_end(toks, 1) == len(toks):
            v = self.expr(toks[2:-1])[0]
            if v.endswith('.into()'):
                v = v[:-len('.into()')]
            return f'fail(e__, ({v}).into())'
        # result helpers
        k = 0
        while k < len(toks) and (re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', toks[k]) or toks[k] == '::'):
            k += 1
        if k > 0 and k < len(toks) and toks[k] == '(' and toks[k - 1] in ('byref_result', 'img_result', 'interval_result', 'part_result', 'varlena_result', 'cstring_result') and group_end(toks, k) >= len(toks) - 1:
            name = toks[k - 1]
            e = group_end(toks, k)
            args = split_toks(toks[k + 1:e - 1], ',')
            rest = toks[e:]
            if rest == ['?']:
                rest = []
            if rest:
                raise Fail('tokens after result helper')
            if name == 'byref_result':
                self.expr(args[0])
                return f'region_copy(r__, {self.expr(args[1])[0]})'
            if name == 'img_result':
                v = self.expr(args[1][1:] if args[1][0] == '&' else args[1])[0]
                return f'region_copy(r__, {v}.as_bytes())'
            if name == 'interval_result':
                v = self.expr(args[1][1:] if args[1][0] == '&' else args[1])[0]
                return f'region_interval(r__, &{v})'
            if name == 'part_result':
                v = self.expr(args[1])[0]
                if self.rettype == 'float8':
                    return f'match {v} {{ adt_timestamp::PartValue::Float(f__) => ok((f__.to_bits() as i64)), adt_timestamp::PartValue::Null => null_ret(), adt_timestamp::PartValue::Numeric(_) => unreachable!() }}'
                return f'match {v} {{ adt_timestamp::PartValue::Numeric(img__) => region_copy(r__, img__.as_bytes()), adt_timestamp::PartValue::Null => null_ret(), adt_timestamp::PartValue::Float(_) => unreachable!() }}'
        # `helper(fcinfo, ..)` returning PgResult<Datum>: inline at the tail
        j = 0
        while j < len(toks) and (re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', toks[j]) or toks[j] == '::'):
            j += 1
        path = ''.join(toks[:j])
        if j < len(toks) and toks[j] == '(' and group_end(toks, j) == len(toks) and path in SCRATCH_HELPERS:
            return self.call(path, split_args(toks[j + 1:-1]), tail=True)[0]
        if j < len(toks) and toks[j] == '(' and group_end(toks, j) == len(toks) and path in self.f['fns'] and '::' not in path:
            args = split_args(toks[j + 1:-1])
            self.want_ret = True
            try:
                text, k = self.inline_fn(path, args, tail=True)
            finally:
                self.want_ret = False
            return text
        if j < len(toks) and toks[j] == '(' and group_end(toks, j) == len(toks) and path in self.f['defs']:
            pass
        if j < len(toks) and toks[j] == '!' and path in self.f['defs']:
            e = group_end(toks, j + 1)
            exp = expand_macro(self.f['defs'], path, toks[j + 2:e - 1])
            if exp is None:
                raise Fail('macro ' + path)
            return self.ret_text(exp + toks[e:])
        # `X.with(|c| ..)` over a thread-local scratch
        if j < len(toks) and toks[j] == '.' and path in self.f['tls'] and toks[j + 1] == 'with':
            self.want_ret = True
            try:
                return self.method(path, ('tl', path), 'with', [a for a in split_toks(toks[j + 3:group_end(toks, j + 2) - 1], ',') if a])[0]
            finally:
                self.want_ret = False
        # `Ok(x?)` shapes were handled in ret_ok; a general PgResult<Datum> expression
        self.questioned = True
        try:
            v, k = self.expr(toks)
        finally:
            self.questioned = False
        if k == 'null':
            return 'null_ret()'
        if k == 'ret':
            return v
        if k not in ('val', 'noq'):
            raise Fail(f'result of kind {k}')
        if toks[-1] == '?':
            # `expr?` of type Datum
            inner = v
            return self.datum_ret(inner)
        if re.match(r'^\{?\s*Ok\(', v):
            v = f'{{ let out__: types_error::PgResult<datum::Datum> = {v}; out__ }}'
        return f'match {v} {{ Ok(d__) => {self.datum_ret("d__")}, Err(err__) => fail(e__, err__.into()) }}'

    def ret_ok(self, inner):
        inner = list(inner)
        while inner and inner[0] == '(' and group_end(inner, 0) == len(inner):
            inner = inner[1:-1]
        if not inner:
            raise Fail('Ok(())')
        if inner[0] == 'unsafe' and inner[1] == '{' and group_end(inner, 1) == len(inner):
            return self.ret_ok(inner[2:-1])
        if inner[0] == '{' and group_end(inner, 0) == len(inner):
            return self.block_ret(inner[1:-1], ok=True)
        if inner[0] == 'if':
            return self.if_expr(inner, True, ok=True)
        if inner[0] == 'match':
            return self.match_expr(inner, True, ok=True)
        j = 0
        while j < len(inner) and (re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', inner[j]) or inner[j] == '::'):
            j += 1
        hp = ''.join(inner[:j])
        if j < len(inner) and inner[j] == '(' and group_end(inner, j) == len(inner) and hp not in ('Some', 'Ok', 'Err') and ((hp in self.f['fns'] and '::' not in hp) or self.find_helper(hp) is not None):
            args = [a for a in split_toks(inner[j + 1:-1], ',') if a]
            other = None if (hp in self.f['fns'] and '::' not in hp) else self.find_helper(hp)
            saved_f = self.f
            if other is not None:
                self.f = other
            try:
                text, k = self.inline_fn(hp.split('::')[-1] if other is not None else hp, args, tail=True)
            finally:
                self.f = saved_f
            return text
        if len(inner) > 2 and inner[1] == '!' and inner[2] in OPEN and inner[0] in self.f['defs']:
            exp = expand_macro(self.f['defs'], inner[0], inner[3:group_end(inner, 2) - 1])
            if exp is None:
                raise Fail('macro ' + inner[0])
            return self.ret_ok(exp + inner[group_end(inner, 2):])
        if inner[:3] == ['Datum', '::', 'null'] or inner[:5] == ['::', 'datum', '::', 'Datum', '::'] and inner[5] == 'null':
            return 'null_ret()'
        if inner[0] == 'fcinfo' and inner[1] == '.' and inner[2] == 'return_null':
            return 'null_ret()'
        if inner[-1] == '?':
            # Ok(E?) ≡ E for a PgResult<Datum> E
            return self.ret_text(inner[:-1])
        # Datum::from_T(v)
        di = 0
        if inner[:3] == ['::', 'datum', '::']:
            di = 3
        if len(inner) > di + 3 and inner[di] == 'Datum' and inner[di + 1] == '::' and inner[di + 3] == '(' and group_end(inner, di + 3) == len(inner):
            ctor = inner[di + 2]
            parts = [x for x in split_toks(inner[di + 4:-1], ',') if x]
            if ctor in CTOR:
                if len(parts) != 1:
                    raise Fail('ctor arity')
                v, k = self.expr(parts[0])
                if k not in ('val', 'noq'):
                    raise Fail(f'ctor over {k}')
                if self.rclass != 'byval':
                    raise Fail(f'by-value constructor {ctor} for a {self.rettype} result')
                return f'ok({self.enc(CTOR[ctor], v)})'
            if ctor in ('from_usize', 'from_ptr'):
                sel = parts[0]
                if sel and sel[0] == 'if' and sel[-2:] == ['as', 'usize']:
                    # `if c { a.as_ptr() } else { b.as_ptr() } as usize`: the chosen argument is the result
                    body = sel[:-2]
                    j = self.find_brace(body, 1)
                    te = group_end(body, j)
                    x = body[j + 1:te - 1]
                    y = body[te + 2:group_end(body, te + 1) - 1] if te < len(body) and body[te] == 'else' else None
                    def packed_of(e):
                        if e and e[-3:] == ['as_ptr', '(', ')'] and e[-4] == '.':
                            _, kk = self.expr(e[:-4])
                            if isinstance(kk, tuple) and kk[0] == 'packed':
                                return kk[1]
                        return None
                    px, py = packed_of(x), packed_of(y) if y is not None else None
                    if px is not None and py is not None:
                        cond = self.cond(body[1:j])
                        return f'if {cond} {{ region_payload(r__, a{px}__.bytes()) }} else {{ region_payload(r__, a{py}__.bytes()) }}'
                v, k = self.expr(parts[0])
                return self.ptr_ret(v)
            raise Fail('Datum::' + ctor)
        k = 0
        while k < len(inner) and (re.match(r'^[A-Za-z_][A-Za-z0-9_]*$', inner[k]) or inner[k] == '::'):
            k += 1
        if k > 0 and k < len(inner) and inner[k] == '(' and group_end(inner, k) == len(inner) and inner[k - 1] == 'cstring_scratch':
            # an out function's cstring in the FmgrInfo's scratch: the shim writes the same bytes
            # plus a NUL into its result region (the flinfo and the name only key that scratch)
            parts = split_toks(inner[k + 1:-1], ',')
            if len(parts) != 3:
                raise Fail('cstring_scratch arity')
            if self.rclass == 'byval':
                raise Fail('cstring_scratch for a by-value result')
            v, kk = self.expr(parts[2])
            v, kk = self.as_value(v, kk)
            if kk not in ('val', 'noq'):
                raise Fail(f'cstring_scratch of {kk}')
            return f'region_cstring(r__, {v})'
        if k > 0 and k < len(inner) and inner[k] == '(' and group_end(inner, k) == len(inner) and inner[k - 1] in ('varlena_result', 'cstring_result'):
            v, kk = self.expr(inner[k + 1:-1])
            if self.rclass == 'byval':
                raise Fail(inner[k - 1] + ' for a by-value result')
            if inner[k - 1] == 'varlena_result':
                return f'region_varlena(r__, {v})'
            return f'region_copy(r__, &{v})'
        if inner[0] == 'fcinfo' and inner[1] == '.' and inner[2] == 'arg' and inner[3] == '(' and group_end(inner, 3) == len(inner):
            i = int(inner[4])
            c = self.cls(i)
            if c == 'view' or c == 'byref' and self.sqlargs[i] == 'numeric':
                return f'region_payload(r__, a{i}__.bytes())'
            if c in ('byref', 'fixed', 'img'):
                return f'region_copy(r__, a{i}__.bytes())'
            if c == 'byval':
                return f'ok(a{i}__)'
            raise Fail('Ok(fcinfo.arg(i)) for a polymorphic element')
        # a general Datum expression
        self.questioned = True
        try:
            v, k = self.expr(inner)
        finally:
            self.questioned = False
        if k == 'null':
            return 'null_ret()'
        if k == 'ret':
            return v
        if isinstance(k, tuple) and k[0] == 'ndv':
            i = k[1]
            return self.ret_ok(['fcinfo', '.', 'arg', '(', str(i), ')'])
        if k not in ('val', 'noq'):
            raise Fail(f'Ok({k})')
        return self.datum_ret(v)

    def enc(self, ty, v):
        """A by-value result `v` of Rust type `ty` -> the shim's `Ret.val` word: WIDEN's encode, except that an int2 / int4
        result is sign-extended at its SQL width whatever the value's Rust type (D2311: the emitted code reads an int2 /
        int4 word sign-extended, prim.rs; a hash body's `u32` through `Datum::from_u32` is PG's `UInt32GetDatum`, zero-
        extended, which C reads through DatumGetInt32 and the open-coded forms read whole)."""
        sql = {'int2': 'i16', 'int4': 'i32'}.get(self.rettype)
        if sql is not None and ty != sql:
            return f'({v} as {sql} as i64)'
        return WIDEN[ty][1].format(v)

    def datum_ret(self, v):
        """A value of type Datum -> Ret, by the result's class."""
        if self.rclass == 'byval':
            ty = byval_ty(self.rettype)
            return f'ok({WIDEN[ty][1].format(f"{v}.{ {v: k for k, v in GETTER.items()}[ty] }()")})'
        return self.ptr_ret(f'{v}.as_usize()')

    def ptr_ret(self, p):
        """A usize pointer to the result image -> a copy into the region."""
        if self.rclass == 'byval':
            return f'ok(({p}) as i64)'
        if self.rclass == 'datum':
            raise Fail('polymorphic element result')
        tl = typlen_of(self.rettype)
        if self.rclass == 'view' and self.rettype == 'cstring':
            tl = -2
        if tl is None:
            raise Fail('result typlen of ' + self.rettype)
        return f'region_datum(r__, ({p}) as *const u8, {tl})'

    # -- the whole wrapper
    def wrapper(self, sig, body):
        """-> dict(prelude, text, coll, mcx, tyargs)."""
        params, _ = self.params_of(sig)
        if len(params) != 2:
            raise Fail('wrapper signature')
        for t in body:
            if t in CONTEXT_METHODS and t != 'context':
                raise Ctx(CONTEXT_METHODS[t])
        fl, fc = params
        self.env[fc] = ('fcinfo', 'fcinfo')
        if not fl.startswith('_'):
            self.env[fl] = ('flinfo', 'flinfo')
        text = self.block_ret(body)
        for w in ('fcinfo', 'flinfo'):
            if re.search(r'\b' + w + r'\b', text):
                raise Fail(w + ' escapes the translation')
        if self.uses_extra and self.extra is None:
            raise Fail('fn_extra used without a typed access')
        return dict(prelude=self.prelude, text=text, coll=self.uses_coll, mcx=self.uses_mcx, tyargs=sorted(self.tyargs), extra=self.extra)


ROOT_ITEMS = {}
MOD_ITEMS = {}


def module_file(path):
    """`crate::a::b` -> the source file of that module, if it can be found."""
    segs = path.split('::')
    d = ALL_CRATES.get(segs[0])
    if not d:
        return None
    rel = os.path.join(d, 'src', *segs[1:])
    for cand in (rel + '.rs', os.path.join(rel, 'mod.rs'), os.path.join(d, 'src', 'lib.rs') if len(segs) == 1 else None):
        if cand and os.path.exists(cand):
            return cand
    return None


def module_items(path):
    """Names a module defines (fns, types, consts) — for glob imports."""
    if path in MOD_ITEMS:
        return MOD_ITEMS[path]
    out = {}
    f = module_file(path)
    if f:
        toks = tokenize(open(f).read())
        out.update(scan_types(toks))
        fns = {}
        scan_fns(toks, fns)
        out.update({k: v[2] for k, v in fns.items()})
        for m in re.finditer(r'pub\s+use\s+[^;]*?\{([^}]*)\}', open(f).read()):
            out.update({x.strip().split(' as ')[-1].strip(): 'pub' for x in m.group(1).split(',') if x.strip()})
    MOD_ITEMS[path] = out
    return out


def crate_root_items(crate):
    """Names defined at a crate's root (lib.rs) or re-exported there with `pub use`."""
    if crate in ROOT_ITEMS:
        return ROOT_ITEMS[crate]
    out = {}
    d = ALL_CRATES.get(crate)
    if d:
        lib = os.path.join(d, 'src', 'lib.rs')
        if os.path.exists(lib):
            toks = tokenize(open(lib).read())
            out.update(scan_types(toks))
            fns = {}
            scan_fns(toks, fns)
            out.update({k: v[2] for k, v in fns.items()})
            for m in re.finditer(r'pub\s+use\s+[^;]*?\{([^}]*)\}', open(lib).read()):
                out.update({x.strip().split(' as ')[-1].strip(): 'pub' for x in m.group(1).split(',') if x.strip()})
            for m in re.finditer(r'pub\s+use\s+[A-Za-z0-9_:]*::([A-Za-z0-9_]+)\s*;', open(lib).read()):
                out[m.group(1)] = 'pub'
    ROOT_ITEMS[crate] = out
    return out


def group_end_angle(toks, i):
    depth, j = 0, i
    while j < len(toks):
        if toks[j] == '<':
            depth += 1
        elif toks[j] == '<<':
            depth += 2
        elif toks[j] in ('>', '>>'):
            depth -= 1 if toks[j] == '>' else 2
            if depth <= 0:
                return j + 1
        j += 1
    return len(toks)


# ---------------------------------------------------------------- visibility of the typed body

VIS = {}


def body_visibility(path):
    if path in VIS:
        return VIS[path]
    segs = path.split('::')
    crate, name = segs[0], segs[-1]
    cdir = CRATE_DIR.get(crate)
    hit = ('unknown', '')
    if cdir:
        for dirpath, dirnames, filenames in os.walk(cdir):
            dirnames[:] = sorted(d for d in dirnames if d != 'target')
            for f in sorted(filenames):
                if not f.endswith('.rs'):
                    continue
                p = os.path.join(dirpath, f)
                src = open(p).read()
                for m in re.finditer(r'^(\s*)(pub(?:\([a-z]+\))?\s+)?(?:const\s+|unsafe\s+|extern "C" )*fn ' + re.escape(name) + r'\b', src, re.M):
                    line = src[:m.start()].count('\n') + 1
                    if m.group(2) and m.group(2).strip() == 'pub':
                        hit = ('pub', f'{os.path.relpath(p, ROOT)}:{line}')
                        break
                    hit = ('private', f'{os.path.relpath(p, ROOT)}:{line}')
                if hit[0] == 'pub':
                    break
            if hit[0] == 'pub':
                break
    VIS[path] = hit
    return hit


FN_TEXT = {}


def fn_source(path):
    """The source text of a fn named by a workspace path (its named module first, else the first definition in sorted file order)."""
    if path in FN_TEXT:
        return FN_TEXT[path]
    segs = path.split('::')
    crate, name = segs[0], segs[-1]
    out = None
    d = ALL_CRATES.get(crate)
    if d:
        pat = re.compile(r'\bfn ' + re.escape(name) + r'\b')
        # the module the path names first (`adt_jsonb::mutate::JsonbPush::new` -> src/mutate.rs), then every
        # file in sorted order: a bare `new` / `push` otherwise resolved to the first `fn new` of os.walk's
        # order, which is the filesystem's (APFS vs ext4 classed jsonb_object differently, 09-26)
        mods = [s for s in segs[1:-1] if re.fullmatch(r'[a-z_][a-z0-9_]*', s)]
        src_dir = os.path.join(d, 'src')
        named = [os.path.join(src_dir, *mods) + '.rs', os.path.join(src_dir, *mods, 'mod.rs')] if mods else []
        walked = []
        for dirpath, dirnames, filenames in os.walk(src_dir):
            dirnames.sort()
            walked += [os.path.join(dirpath, f) for f in sorted(filenames) if f.endswith('.rs')]
        for f in [q for q in named if os.path.isfile(q)] + [q for q in walked if q not in named]:
            src = open(f).read()
            m = pat.search(src)
            if m:
                i = src.find('{', m.end())
                depth, j = 0, i
                while j < len(src):
                    if src[j] == '{':
                        depth += 1
                    elif src[j] == '}':
                        depth -= 1
                        if depth == 0:
                            break
                    j += 1
                out = src[m.start():j + 1]
                break
    FN_TEXT[path] = out
    return out


CATALOG_MARKS = tuple(c + '::' for c in sorted(CATALOG_CRATES)) + ('syscache_seams::', 'SearchSysCache', 'lookup_type_cache', 'get_typlenbyvalalign', 'getTypeOutputInfo', 'get_type_io_data')
# Cold helpers whose only catalog read names a type in a RAISED error's text (C's format_type_be on an error path): no
# value a shim returns reads the catalog, so a body reaching the catalog only through one keeps its shim. Each one asks
# catcache_seams::catalog_readable_here() first and, on a pool thread (no catcache, D1409), takes the type's own name
# with no catalog read. Reviewed by hand, one entry each.
ERROR_TEXT_CATALOG = {
    'adt_date::date_type_name',  # date.c format_type_be(DATEOID) in extract_date's unit errors (pgrust main 56e96af3b8c84, D3128)
}


def body_reaches_catalog(path, depth=0):
    """A catalog read inside the typed body (two levels of same-crate calls)."""
    src = fn_source(path)
    if src is None or depth > 2 or path in ERROR_TEXT_CATALOG:
        return None
    for mk in CATALOG_MARKS:
        if mk in src:
            return mk.rstrip(':')
    crate = path.split('::')[0]
    callees = set()
    for m in re.finditer(r'\b((?:crate|' + re.escape(crate) + r')::[a-z_][A-Za-z0-9_:]*)\(', src):
        callees.add(m.group(1).replace('crate::', crate + '::', 1))
    # bare calls to functions of the same crate (`json_categorize_type(..)`)
    for m in re.finditer(r'(?<![A-Za-z0-9_:.])([a-z_][a-z0-9_]*)\(', src):
        nm = m.group(1)
        if nm in ('if', 'while', 'match', 'for', 'return', 'unsafe', 'loop', 'fn', 'as', 'in') or nm.startswith('debug_assert'):
            continue
        callees.add(crate + '::' + nm)
    for p in sorted(callees):
        if p != path and fn_source(p) is not None:
            hit = body_reaches_catalog(p, depth + 1)
            if hit:
                return hit
    return None


# ---------------------------------------------------------------- classification

NOT_PORTED = None
np_path = os.path.join(FN, 'not-ported.txt')
if os.path.exists(np_path):
    NOT_PORTED = {int(l.split()[0]) for l in open(np_path) if l.strip() and not l.startswith('#')}
else:
    sys.stderr.write('warning: no crates/gravity/fn/not-ported.txt (GRAVITY_TIERS_UPDATE=1 cargo test -p gravity_fn --test tiers writes it)\n')


def is_session(name):
    if name.startswith('pg_lsn'):
        return False
    return name in SESSION_NAMES or any(name.startswith(p) for p in SESSION_PREFIXES)


def family_of(acls, rcls):
    a = set(acls)
    if rcls == 'byval':
        if a <= {'byval', 'datum'}:
            return 'byval'
        if 'view' in a:
            return 'view_val'
        return 'byref_val'
    if a <= {'byval', 'datum'}:
        return 'val_view'
    return 'byref_ref'


def translate(oid, prosrc, sqlargs, rettype):
    reg = REGISTRY.get(oid)
    if reg is None:
        return None, 'no registry row', None
    wname, path = reg
    f = FILES[path]
    if '::' in wname:
        head = CRATE_RENAMES.get(f['crate'], {}).get(wname.split('::')[0], wname.split('::')[0])
        head = CRATE_ALIAS.get(head, head)
        for other, of in FILES.items():
            if of['crate'] == head and wname.split('::')[-1] in of['fns']:
                f = of
                break
    wname = wname.split('::')[-1]
    if wname not in f['fns']:
        for other, of in FILES.items():
            if of['crate'] == f['crate'] and wname in of['fns'] and wname in of['locals']:
                f = of
                break
    if wname not in f['fns']:
        return None, 'wrapper ' + wname + ' not in ' + os.path.basename(path), None
    sig, body = f['fns'][wname][:2]
    tr = Translator(f, sqlargs, rettype, GEN_ACLS.get(prosrc))
    if os.environ.get('GRAVITY_TIERS_DEBUG') == str(oid):
        sys.stderr.write('wrapper ' + wname + ': ' + join(body) + '\n')
    try:
        try:
            out = tr.wrapper(sig, body)
        except Fail as ex:
            if 'fn_extra used before' not in str(ex):
                raise
            extra = tr.extra
            if extra is None:
                seen = set()
                for scope in ([f], [o for o in FILES.values() if o['crate'] == f['crate']]):
                    for ff in scope:
                        tk = ff['toks']
                        for i in range(len(tk) - 4):
                            if tk[i] in ('fn_extra_mut', 'fn_extra_ref') and tk[i + 1] == '::' and tk[i + 2] == '<':
                                seen.add(join(tk[i + 3:group_end_angle(tk, i + 2) - 1]))
                    if len(seen) == 1:
                        break
                if len(seen) != 1:
                    raise
                extra = tr.extra_type(seen.pop())
            tr = Translator(f, sqlargs, rettype, GEN_ACLS.get(prosrc))
            tr.extra = extra
            out = tr.wrapper(sig, body)
    except Ctx as ex:
        return None, None, str(ex)
    except Fail as ex:
        if os.environ.get('GRAVITY_TIERS_DEBUG') == str(oid):
            import traceback
            traceback.print_exc()
            sys.exit(1)
        return None, str(ex), None
    except (IndexError, ValueError, KeyError) as ex:
        if os.environ.get('GRAVITY_TIERS_DEBUG') == str(oid):
            import traceback
            traceback.print_exc()
            sys.exit(1)
        return None, 'parse: ' + type(ex).__name__ + ' ' + str(ex)[:40], None
    except RecursionError:
        return None, 'parse: recursion', None
    out['crate'] = f['crate']
    if os.environ.get('GRAVITY_TIERS_DEBUG') == str(oid):
        sys.stderr.write('shim: ' + ' '.join(out['prelude']) + ' ' + out['text'] + '\n')
        sys.exit(1)
    return out, None, None


# The set-returning function library (docs/gravity/lower.md §12; crates/gravity/fn/src/srf.rs
# `ENTRIES` + `EMITTED`, test-enforced by tests/srf_differential.rs): status `srf`, tier B — the
# lowered SRF loop open-codes the iterator or calls the entry's open / step, never the fmgr.
SRF_LIBRARY = {
    1066: 'emitted (the integer series, srflib.rs)', 1067: 'emitted (the integer series, srflib.rs)',
    1068: 'emitted (the integer series, srflib.rs)', 1069: 'emitted (the integer series, srflib.rs)',
    1191: 'emitted (generate_subscripts, srflib.rs)', 1192: 'emitted (generate_subscripts, srflib.rs)',
    2331: 'emitted (the array walk, srflib.rs)',
    938: 'gravity_fn::srf::series_time_open', 939: 'gravity_fn::srf::series_time_open', 6274: 'gravity_fn::srf::series_time_open',
    3259: 'gravity_fn::srf::series_numeric_open', 3260: 'gravity_fn::srf::series_numeric_open',
    2763: 'gravity_fn::srf::regexp_matches_open', 2764: 'gravity_fn::srf::regexp_matches_open',
    2765: 'gravity_fn::srf::regexp_split_open', 2766: 'gravity_fn::srf::regexp_split_open',
    6160: 'gravity_fn::srf::string_to_table_open', 6161: 'gravity_fn::srf::string_to_table_open',
    3219: 'gravity_fn::srf::jsonb_elements_open', 3465: 'gravity_fn::srf::jsonb_elements_open',
    3208: 'gravity_fn::srf::jsonb_each_open', 3932: 'gravity_fn::srf::jsonb_each_open',
    3931: 'gravity_fn::srf::jsonb_keys_open',
    3955: 'gravity_fn::srf::json_elements_open', 3969: 'gravity_fn::srf::json_elements_open',
    3958: 'gravity_fn::srf::json_each_open', 3959: 'gravity_fn::srf::json_each_open',
    3957: 'gravity_fn::srf::json_keys_open',
    4006: 'gravity_fn::srf::jsonb_path_query_open', 1179: 'gravity_fn::srf::jsonb_path_query_open',
}

LINKED = set(re.findall(r'^([a-z_0-9]+)\s*=\s*\{', open(os.path.join(FN, 'Cargo.toml')).read().split('[dev-dependencies]')[0], re.M))
LINKED |= {'adt_numeric', 'types_fmgr', 'datum', 'mcx', 'types_core', 'types_error', 'types_tuple', 'core', 'std', 'alloc'}
rows, bodies = [], {}
PENDING_WHY, PENDING_LIST, PRIVATE, CONTEXT_LIST = Counter(), [], {}, []
CALLED = set()
for oid, prosrc, cnargs, cstrict, cretset in CANONICAL:
    p = PROCS.get(oid)
    ported = '?' if NOT_PORTED is None else ('stub' if oid in NOT_PORTED else 'ported')
    if p is None:
        rows.append((oid, prosrc, '?', 'C', '-', 'fmgr', 'context: session/transaction state (no pg_proc row in the reference for this OID)', ported))
        continue
    name = p['proname']
    args = p.get('proargtypes', '').split()
    ret = p['prorettype']
    strict = p.get('proisstrict', 't') == 't'
    vol = p.get('provolatile', 'i')
    kind = p.get('prokind', 'f')
    retset = p.get('proretset', 'f') == 't'
    acls = [arg_class(a) for a in args]
    rcls = arg_class(ret)
    cls = ('/'.join(a or 'x' for a in acls) or '-') + '->' + (rcls or 'x')
    fam = family_of(acls, rcls) if rcls and all(acls) else '-'

    def C(reason):
        rows.append((oid, prosrc, cls, 'C', '-', 'fmgr', reason, ported))

    if retset and oid in SRF_LIBRARY:
        rows.append((oid, prosrc, cls, 'B', '-', 'srf', 'srf: ' + SRF_LIBRARY[oid], ported))
        continue
    if retset:
        C('set-returning: ReturnSetInfo and the SRF contexts')
    elif kind == 'w':
        C('window: the WindowObject in fcinfo->context')
    elif kind == 'p':
        C('procedure: CALL only')
    elif kind == 'a':
        C('aggregate')
    elif any(t in HANDLER for t in args + [ret]) or 'void' in args:
        C('handler/internal: support function, AM handler or trigger')
    elif ret == 'void':
        C('context: a void result (the call is its side effect; the fmgr wrapper returns the zero datum)')
    elif not strict:
        C('non-strict: the null semantics live in the body')
    elif any(m in p.get('proargmodes', '') for m in ('o', 't', 'b')):
        C('polymorphic/record: OUT arguments (a record result)')
    elif p.get('provariadic') == 'any':
        C('polymorphic/record: VARIADIC "any" (arity and types from fn_expr)')
    elif ret in POLY_ELEM:
        C('polymorphic/record: an element-type result resolved per call')
    elif ret == 'record' or ret == 'unknown':
        C('polymorphic/record: ' + ret + ' result')
    elif not (rcls and all(acls)):
        bad = [t for t in args + [ret] if arg_class(t) is None]
        C('handler/internal: ' + bad[0])
    elif (is_session(name) or is_session(prosrc)) and prosrc not in HAND_SESSION:
        C('context: session/transaction state (' + name + ')')
    elif prosrc in KEEP_C:
        C(KEEP_C[prosrc])
    elif ported == 'stub':
        C('stub: the builtin is not ported')
    elif oid in REGISTRY and FILES[REGISTRY[oid][1]]['crate'] in CATALOG_HOST_CRATES:
        C('context: catalog (' + CATALOG_HOST_CRATES[FILES[REGISTRY[oid][1]]['crate']] + ')')
        CONTEXT_LIST.append((oid, prosrc, cls, 'catalog (' + CATALOG_HOST_CRATES[FILES[REGISTRY[oid][1]]['crate']] + ')'))
    else:
        tier = 'A' if prosrc in OPEN_CODED else 'B'
        if prosrc in HAND:
            rows.append((oid, prosrc, cls, tier, fam, 'shim', 'hand adapter: shims/' + HAND[prosrc] + '.rs', ported))
            continue
        if prosrc in PENDING_NAMES:
            PENDING_WHY['hand'] += 1
            PENDING_LIST.append((oid, prosrc, cls, PENDING_NAMES[prosrc]))
            rows.append((oid, prosrc, cls, tier, fam, 'pending', 'typed body not extractable: ' + PENDING_NAMES[prosrc], ported))
            continue
        body, why, ctx = translate(oid, prosrc, args, ret)
        if ctx is not None:
            C('context: ' + ctx)
            CONTEXT_LIST.append((oid, prosrc, cls, ctx))
            continue
        if body is None:
            PENDING_WHY[why.split(' ')[0]] += 1
            PENDING_LIST.append((oid, prosrc, cls, why))
            rows.append((oid, prosrc, cls, tier, fam, 'pending', 'typed body not extractable: ' + why, ported))
            continue
        status = 'shim-copy' if body['mcx'] else 'shim'
        text_all = ' '.join(body['prelude']) + ' ' + body['text']
        foreign = sorted({m.group(1) for m in re.finditer(r'(?<![A-Za-z0-9_:])([a-z_][a-z0-9_]*)::', text_all)
                          if m.group(1) in ALL_CRATES and m.group(1) not in LINKED})
        catalog = sorted({m.group(1) for m in re.finditer(r'\b([a-z_][a-z0-9_]*)::', text_all) if m.group(1) in CATALOG_CRATES})
        if not catalog:
            for m in re.finditer(r'\b([a-z_][a-z0-9_]*(?:::[a-z_][A-Za-z0-9_]*)+)\(', text_all):
                hit = body_reaches_catalog(m.group(1))
                if hit:
                    catalog = [m.group(1) + ' -> ' + hit]
                    break
        if catalog:
            C('context: catalog (' + catalog[0] + ')')
            CONTEXT_LIST.append((oid, prosrc, cls, 'catalog (' + catalog[0] + ')'))
            continue
        if foreign:
            PENDING_WHY['crate'] += 1
            PENDING_LIST.append((oid, prosrc, cls, 'crate ' + foreign[0] + ' not linked by gravity_fn'))
            rows.append((oid, prosrc, cls, tier, fam, 'pending', 'typed body not extractable: crate ' + foreign[0] + ' not linked', ported))
            continue
        for m in re.finditer(r'\b([a-z_][a-z0-9_]*(?:::[a-z_][A-Za-z0-9_]*)+)\(', text_all):
            path = m.group(1)
            if path.split('::')[0] in CRATE_DIR:
                CALLED.add(path)
                vis, where = body_visibility(path)
                if vis == 'private':
                    PRIVATE[path] = where
        key = prosrc
        bacls = GEN_ACLS.get(prosrc) or acls
        if key in bodies and bodies[key]['sig'] != (tuple(bacls), rcls):
            key = f'{prosrc}_{oid}'
        b = bodies.setdefault(key, dict(body, sig=(tuple(bacls), rcls), fam=fam, tier=tier, oids=[],
                                        status=status, sqlargs=args, prosrc=prosrc, acls=bacls,
                                        has_coll=body['coll'] or any(t in COLLATABLE for t in args)))
        b['oids'].append(oid)
        rows.append((oid, prosrc, cls, tier, fam, status, 'shims/' + fam + '.rs::' + key, ported))

# ---------------------------------------------------------------- emit: the table and tier.rs

os.makedirs(os.path.join(FN, 'src', 'shims'), exist_ok=True)
pending_n = sum(1 for r in rows if r[5] == 'pending')
with out_file(os.path.join(FN, 'function_tiers.tsv')) as out:
    out.write('# GENERATED by scripts/gen-gravity-function-tiers.py (docs/gravity/expr.md §2.1). Do not hand-edit.\n')
    out.write(f'# pending {pending_n}\n')
    out.write('oid\tname\tclass\ttier\tfamily\tstatus\treason\tported\n')
    for r in rows:
        out.write('\t'.join(str(x) for x in r) + '\n')

FAM = {'byval': 'ByVal', 'byref_val': 'ByRefVal', 'byref_ref': 'ByRefRef', 'view_val': 'ViewVal', 'val_view': 'ValView', '-': 'None'}
ST = {'shim': 'Shim', 'shim-copy': 'ShimCopy', 'pending': 'Pending', 'fmgr': 'Fmgr', 'srf': 'Srf'}
with out_file(os.path.join(FN, 'src', 'tier.rs')) as out:
    out.write('//! GENERATED by scripts/gen-gravity-function-tiers.py from function_tiers.tsv. Do not hand-edit.\n')
    out.write('use crate::{Family, Row, Status, Tier};\n\n')
    out.write('pub const PENDING: usize = %d;\n\n' % pending_n)
    out.write('pub static ROWS: [Row; %d] = [\n' % len(rows))
    for oid, name, cls, tier, fam, status, reason, ported in rows:
        out.write('    Row { oid: %d, name: "%s", tier: Tier::%s, family: Family::%s, status: Status::%s, reason: "%s", stub: %s },\n'
                  % (oid, name, tier, FAM[fam], ST[status], reason.replace('"', '\\"'), 'true' if ported == 'stub' else 'false'))
    out.write('];\n')

# ---------------------------------------------------------------- emit: the shim families


def shim_params(b):
    params = ['e__: *mut ErrSlot']
    if b['fam'] in ('val_view', 'byref_ref'):
        params.append('r__: *mut Region')
    if b['has_coll']:
        params.append('coll__: u32')
    for t in b['tyargs']:
        params.append('tr__: u32' if t == -1 else f't{t}__: u32')
    for i, c in enumerate(b['acls']):
        params.append(f'a{i}__: i64' if c in ('byval', 'datum') else f'a{i}__: View')
    return params


def emit_family(fam, path):
    n = 0
    with out_file(path) as out:
        out.write('//! GENERATED by scripts/gen-gravity-function-tiers.py: the %s shim family (docs/gravity/expr.md §2.1).\n' % fam)
        out.write('//! Each shim calls the builtin\'s typed body directly; no fcinfo, no palloc; errors into the caller\'s slot.\n')
        out.write('#![allow(unused_imports, unused_parens, unused_variables, unused_unsafe, unused_mut, unused_braces, unreachable_code, clippy::all, non_snake_case)]\n')
        out.write('use crate::{fail, null_ret, ok, region_copy, region_cstring, region_datum, region_interval, region_payload, region_varlena, with_scratch, ErrSlot, Region, Ret, View};\nuse adt_geo::Pts;\n\n')
        for key in sorted(bodies, key=lambda k: bodies[k]['oids'][0]):
            b = bodies[key]
            if b['fam'] != fam:
                continue
            lines = list(b['prelude']) + ['unsafe { ' + b['text'] + ' }']
            tagged = []
            for l in lines:
                if any(pt in l for pt in ALLOC_PATTERNS):
                    stripped = re.sub(r'Box::new\(\s*(types_error::)?PgError::(error|warning|notice)\(|Box::new\(\s*(types_error::)?PgError::new\(\s*(types_error::)?ERROR,', '', l)
                    stripped = re.sub(r'format!\([^;]*?\)\)\.with_sqlstate', '', stripped)
                    stripped = re.sub(r'PgError::error\(format!\(', '', stripped)
                    cold = not any(pt in stripped for pt in ALLOC_PATTERNS)
                    if not cold:
                        EXEC_ALLOC.add(key)
                    l = l + ('  // alloc: Cold' if cold else '  // alloc: Exec')
                tagged.append(l)
            body_txt = '\n'.join('    ' + l for l in tagged)
            if b['mcx']:
                body_txt = '    with_scratch(|mcx__| {\n' + '\n'.join('    ' + l for l in body_txt.split('\n')) + '\n    })'
            if b.get('extra'):
                out.write(f'pub use {key}_m__::{key};\nmod {key}_m__ {{\nuse super::*;\n')
                out.write('thread_local! {\n    // alloc: Node\n    static EXTRA__: core::cell::UnsafeCell<Option<%s>> = core::cell::UnsafeCell::new(None);\n}\n' % b['extra'])
                out.write(f'pub extern "C-unwind" fn {key}({", ".join(shim_params(b))}) -> Ret {{\n{body_txt}\n}}\n}}\n\n')
            else:
                out.write(f'pub extern "C-unwind" fn {key}({", ".join(shim_params(b))}) -> Ret {{\n{body_txt}\n}}\n\n')
            n += 1
    return n


ALLOC_PATTERNS = ['Vec::new(', 'vec![', 'Box::new(', 'String::from(', '.to_string()', 'format!(']
EXEC_ALLOC = set()
FAMILY_FILES = ['byval', 'byref_val', 'byref_ref', 'view_val', 'val_view']
fam_n = {f: emit_family(f, os.path.join(FN, 'src', 'shims', f + '.rs')) for f in FAMILY_FILES}

entries = []
for key, b in bodies.items():
    for o in b['oids']:
        entries.append((o, b['prosrc'], b['fam'], b['fam'], key, b['acls'], b['has_coll'], b['tyargs']))
for oid, name, cls, tier, fam, status, reason, ported in rows:
    if status == 'shim' and reason.startswith('hand adapter'):
        sa = PROCS[oid].get('proargtypes', '').split()
        entries.append((oid, name, fam, HAND[name], name, HAND_ACLS.get(name) or [arg_class(t) for t in sa], any(t in COLLATABLE for t in sa), []))
entries.sort()
with out_file(os.path.join(FN, 'src', 'shims', 'catalog.rs')) as out:
    out.write('//! GENERATED by scripts/gen-gravity-function-tiers.py: every shim by OID (ascending). Do not hand-edit.\n')
    out.write('#![allow(clippy::all, unused_variables)]\nuse crate::shims::*;\nuse crate::{Addr, ArgKind, ErrSlot, Family, RawFn, Region, Ret, Shim, View};\n\n')
    out.write('pub static SHIMS: [Shim; %d] = [\n' % len(entries))
    KIND = {'byval': 'V', 'datum': 'D', 'view': 'P', 'byref': 'P', 'fixed': 'I', 'img': 'I'}
    for o, name, fam, module, key, acls, coll, tyargs in entries:
        region = fam in ('val_view', 'byref_ref')
        call_args = ['e'] + (['r'] if region else []) + (['coll'] if coll else []) + [f'ty[{n}]' for n in range(len(tyargs))] + [
            f'a[{i}].v()' if c in ('byval', 'datum') else f'a[{i}].r()' for i, c in enumerate(acls)]
        ptys = ['*mut ErrSlot'] + (['*mut Region'] if region else []) + (['u32'] if coll else []) + ['u32'] * len(tyargs) + [
            'i64' if c in ('byval', 'datum') else 'View' for c in acls]
        fty = 'extern "C-unwind" fn(' + ', '.join(ptys) + ') -> Ret'
        kinds = ', '.join('ArgKind::' + KIND[c] for c in acls)
        out.write(f'    Shim {{ oid: {o}, name: "{name}", family: Family::{FAM[fam]}, nargs: {len(acls)}, coll: {str(coll).lower()}, '
                  f'region: {str(region).lower()}, args: &[{kinds}], tyargs: &[{", ".join(str(t) for t in tyargs)}], '
                  f'addr: Addr(unsafe {{ core::mem::transmute::<{fty}, RawFn>({module}::{key}) }}), '
                  f'call: |e, r, coll, ty, a| {module}::{key}({", ".join(call_args)}) }},\n')
    out.write('];\n')

# ---------------------------------------------------------------- the report
tiers = Counter(r[3] for r in rows)
status = Counter((r[3], r[5]) for r in rows)
fams = Counter((r[4], r[5]) for r in rows if r[3] in 'AB')
reasons = Counter(r[6].split(':')[0] for r in rows if r[3] == 'C')
sub = Counter(r[6].split('(')[0].strip() for r in rows if r[3] == 'C' and r[6].startswith('context'))
crates_used = Counter(p.split('::')[0] for p in CALLED)
e = sys.stderr
e.write(f'{len(rows)} canonical builtins: ' + ', '.join(f'{k} {tiers[k]}' for k in 'ABC') + '\n')
for k in 'AB':
    e.write(f'  {k}: ' + ', '.join(f'{s} {n}' for (t, s), n in sorted(status.items()) if t == k) + '\n')
e.write('  families: ' + ', '.join(f'{f}/{s} {n}' for (f, s), n in sorted(fams.items())) + '\n')
e.write('  C by reason: ' + ', '.join(f'{c} {n}' for c, n in reasons.most_common()) + '\n')
e.write('  context by kind: ' + ', '.join(f'{c} {n}' for c, n in sub.most_common()) + '\n')
e.write('  generated shims per family: ' + ', '.join(f'{f} {n}' for f, n in fam_n.items()) + f'; catalog {len(entries)}\n')
e.write('  crates the shims call: ' + ', '.join(f'{c} {n}' for c, n in crates_used.most_common()) + '\n')
e.write(f'  pending {pending_n}: ' + ', '.join(f'{w} {n}' for w, n in PENDING_WHY.most_common()) + '\n')
e.write(f'  shims with a per-call heap allocation inherited from the wrapper (alloc: Exec): {len(EXEC_ALLOC)}\n')
if PRIVATE:
    e.write(f'  private typed bodies ({len(PRIVATE)}; make pub):\n')
    for p, where in sorted(PRIVATE.items()):
        e.write(f'    {p}  {where}\n')
if os.environ.get('GRAVITY_TIERS_PENDING'):
    for o, n, c, w in PENDING_LIST:
        e.write(f'    pending {o} {n} {c}: {w}\n')
if os.environ.get('GRAVITY_TIERS_CONTEXT'):
    for o, n, c, w in CONTEXT_LIST:
        e.write(f'    context {o} {n} {c}: {w}\n')
if CHECK:
    if CHECK_STALE:
        e.write('STALE (regenerate with scripts/gen-gravity-function-tiers.py): ' + ' '.join(CHECK_STALE) + '\n')
        sys.exit(1)
    e.write('tier table and shims agree with the generator\n')

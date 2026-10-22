# Carve ratifications — permanently-out-of-scope boundaries

Status: §1–§2 RATIFIED by Michael, 2026-08-06; §1 RETIRED 2026-09-17 (native initdb landed, see below) (recorded by the coordinator;
this document is the ruling of record). Prior state: these were *postures* —
consistent tree-wide fencing with no ruling cited (unported-census
2026-08-05 §0 flagged them `POSTURE ... pending formal ratification`; the
2026-08-01 stub ratification in docs/verification/phase1-carve-census.md
covers fuzz-harness state pins only and does NOT ratify any tree fence).

Consolidation 2026-08-18 (docs lane): §4–§10 record the remaining de-facto
product carves that audits keep re-discovering (0A000 audit lane 19 in
notes/audits/pgrust-specific-0a000-2026-08-18.md; panic-inventory bucket F in
notes/audits/unported-panic-inventory-2026-08-08.md) as numbered rulings, so
future sweeps cite a section instead of re-deriving the posture. They are
recorded-as-standing, pending Michael's countersign; each names its guard
sites, exact user-visible behavior, and re-open trigger. The formerly open
architectural question — non-UTF8 server encodings — was RATIFIED 2026-08-18
by Jordyn (Option A, UTF-8-only); see §11 and
notes/design/non-utf8-encodings-question.md.

A **ratified carve** is a boundary pgrust will not implement, ever, absent
its re-open trigger firing: the fenced sites are permanent, their guards are
product surface (clean C-taxonomy errors, never panics on user-reachable
paths), and no port lane should be chartered against them. Regression pins:
`scripts/carve-fences-e2e.sh`.

## §1 initdb-via-C-postgres — RETIRED 2026-09-17

**Was.** pgrust did not implement bootstrap (`--boot`) mode; cluster
creation used stock PostgreSQL's initdb (run stock `initdb`, then start
pgrust on the resulting data directory). Fenced site: the `--boot` dispatch
arm in `crates/backend/main/main_main/src/lib.rs` refused with
`postgres: bootstrap mode (--boot/--check) is not supported by pgrust`.

**Re-open trigger fired.** Michael, 2026-09-17: standalone distribution is
the end state. Retired by the native-initdb program
(docs/design/native-initdb.md): `postgres --boot` is ported
(`crates/backend/bootstrap/bootstrap`, a BKI interpreter over
bootstrap.c's semantics), BootStrapXLOG lives in transam_xlog, the
formerly unreachable bootstrap-mode arms in catalog_heap / catalog_index /
catalog_toasting / relcache are live code, and `crates/bin/initdb` is a
port of initdb.c that drives the pgrust binary over the share tree staged
beside it. Verification: `scripts/native-boot-e2e.sh` (C vs pgrust
`--boot` on the same BKI, byte comparison of every relation file) and
`scripts/native-initdb-e2e.sh` (C initdb vs pgrust initdb: file sets,
pg_control shape, every pg_catalog table's contents, and a DDL / DML /
plpgsql / CREATE DATABASE / restart smoke on the native cluster).

A cluster made by stock PostgreSQL 18 initdb remains a valid pgrust
cluster (same on-disk format); nothing about running one changed.

## §2 no-dlopen / no C-extension loading — RATIFIED

**Boundary.** pgrust never loads C shared objects. There is no dlopen path
anywhere in the tree: `probin` is a registry key resolved against in-tree
code, never a file. Extensions are in-tree Rust ports (pgcrypto, pg_trgm,
pgvector, bloom precedent); further procedural languages, index/table AMs,
and archive modules arrive the same way.

**Rationale.** The thread-based backend architecture makes process-model C
extensions unsafe to load: C extension code assumes a private process
(globals, signals, longjmp error handling, process-lifetime memory), none of
which holds inside a pgrust backend thread.

**Fenced sites.** DDL-time fences refuse first, so the catalog never carries
an object that cannot dispatch; resolver-level backstops behind them stay
loud for catalogs written outside the fences.

| site | fence | user-visible behavior |
|---|---|---|
| `crates/backend/commands/proclang/src/lib.rs:79-107` (CreateProceduralLanguage) | CREATE [OR REPLACE] LANGUAGE with a handler that is not a registered in-tree PL entry point | `ERROR: 0A000: language handler function <name> is not supported` + detail. New with this ratification (the panic sat downstream in fmgr). Sits after the INLINE/VALIDATOR lookups so C's own error precedence (proclang.c:73-100) is kept (audit-18.6 b214). |
| `crates/backend/commands/amcmds/src/lib.rs:175-208` (lookup_am_handler_func) | CREATE ACCESS METHOD (INDEX or TABLE) with a handler outside the closed in-tree AM set | `ERROR: 0A000: access method handler function <name> is not supported` + detail. New with this ratification (the panics sat downstream in amapi / relcache). Built-in handlers still compose: AM aliases over `heap_tableam_handler`/`bthandler`, and the in-tree extension AMs (`hnswhandler`, `blhandler` via CREATE EXTENSION) pass. |
| `crates/backend/utils/fmgr/fmgr_core/src/lib.rs:556-586` (fmgr_info_pg_proc, other-language arm) | calling a function whose language handler is unregistered (catalog predates the fence) | clean `0A000` error; was `panic!("language handler ... not ported")`, converted. |
| `crates/backend/utils/fmgr/fmgr_core/src/lib.rs` C-language arm + `crates/backend/utils/fmgr/dfmgr/src/lib.rs` | `CREATE FUNCTION ... LANGUAGE C` over an unregistered library; `LOAD` | C's own taxonomy: `58P01 could not access file "<lib>"` (the dlopen stat miss) at CREATE time via fmgr_c_validator; unchanged, verified clean. |
| `crates/backend/access/index/amapi/src/lib.rs:401,407` (unported_handler / unported_translate) | none — backstop behind the amcmds fence | panic citing this document; unreachable from SQL once the fence holds (`known_index_am_handler`, amapi:384, is the shared closed-set probe). Same status: the non-builtin-AM backstops in `crates/backend/utils/cache/lsyscache/src/amop.rs`. |
| `crates/backend/utils/cache/relcache/src/build.rs:52-71` (RelationInitTableAccessMethod) | none — backstop behind the amcmds fence | clean `0A000` error (was a panic, converted); only heap_tableam_handler is carried, pgrcolumnar resolves by pg_am.amname (docs/design/pgrcolumnar-impl.md §7.1). |
| `crates/backend/postmaster/pgarch/src/lib.rs:163-192` (load_archive_library) | `archive_library` GUC set to any module | archiver FATAL `0A000: loading archive module "<lib>" is not supported` + detail; deliberately an ERROR-channel FATAL (exit 1, reaper-accepted, matching C's FATAL-and-relaunch), never a panic — a panic here would cycle the whole cluster. Cluster keeps serving; verified. |
| `crates/backend/libpq/auth_oauth/src/lib.rs` (load_validator_library) | `oauth` HBA line whose `validator` (or the sole `oauth_validator_libraries` entry) is not a registered in-tree validator | C's own taxonomy: `58P01 could not access file "<lib>"` at connect (the dlopen stat miss). The names that DO resolve are in-tree Rust: `jwt_validator` (`crates/backend/libpq/oauth_validators`, product build: JWS/JWKS + RFC 7662 introspection, `jwt_validator.*` GUCs) and, only under main_main's `oauth-test-validator` cargo feature, the C regression module's `validator` / `fail_validator` / `oauth_test_validator` — a build feature, not a GUC, so a product binary cannot be configured into accepting forged tokens (notes/oauth-validator-lane.md). |
| `crates/backend/commands/functioncmds/src/lib.rs:1051-1062` (CREATE FUNCTION language gate) | `CREATE FUNCTION ... LANGUAGE <pl>` beyond sql/internal/c/plpgsql | `ERROR: 0A000: language "<pl>" is not supported yet` — "yet" is deliberate: further PLs are an open port program (in-tree Rust ports), only their arrival *via dlopen* is carved. |

**Re-open trigger.** A native Rust plugin API being chartered. (That API
would be a new, thread-safe contract — it does not resurrect the C ABI.)

**Scope note (ICU exempt).** The carve fences *Postgres-ABI extension code*.
`crates/backend/utils/adt/pg_locale/src/icu_ffi.rs` dlopens the system
libicu (i18n + common, version-suffix probing, dfmgr-style) and that is
deliberately outside this carve: libicu is a thread-safe system library
with no Postgres ABI, no backend globals, and no longjmp error handling —
none of the §2 rationale applies. Same status for any future system-library
binding of that shape.

## §2a platform carves — features other platforms' C builds cannot have

Not scope decisions of ours: C itself only compiles these on their one
platform, so a macOS/Linux pgrust matching the C build for its own platform
never carries them. Recorded 2026-08-18 (auth PAM/GSSAPI lane) alongside the
eventlog precedent.

**SSPI (Windows-only in C).** `ENABLE_SSPI` is defined in exactly one place,
the Windows port header — `cpg-src/src/include/port/win32_port.h:23`
(`#define ENABLE_SSPI 1`, comment "Always build with SSPI support") — with
no configure/meson switch on Unix; every SSPI site (`auth.c pg_SSPI_recvauth`,
`hba.c:1715` method parse) is `#ifdef ENABLE_SSPI`. pgrust keeps the
`sspi` hba method as C's Unix builds do: `invalid authentication method
"sspi": not supported by this build` at hba load. The sspi-only options
(`compat_realm`, `upn_username`) keep their C option-validity errors.

**BSD Authentication (OpenBSD-only in C).** `USE_BSD_AUTH` exists only via
`--with-bsd-auth` (`cpg-src/configure.ac:913`; meson `bsd_auth` option,
`meson.build:566-572`) and configure hard-fails without `<bsd_auth.h>`
(`configure.ac:1601-1602`) — the OpenBSD authentication framework header;
the docs state it "is only available on OpenBSD"
(`doc/src/sgml/client-auth.sgml:2327`). `auth.c CheckBSDAuth` is
`#ifdef USE_BSD_AUTH`. pgrust keeps the `bsd` hba method rejection:
`invalid authentication method "bsd": not supported by this build`.

**Re-open trigger.** A Windows (SSPI) or OpenBSD (bsd) pgrust target being
chartered.

## §3 single-user mode — NOT ratified: PORT it

Michael explicitly chose **not** to carve single-user mode; a separate port
lane is being chartered. The posture is **port-pending, not a carve**: the
`DestDebug` / whereToSendOutput arms recorded as skip items in the
2026-08-01 census (e.g. `commands/copy/src/to.rs`, `from.rs`) are gaps owed
to that lane, and fences guarding them must not cite this document as a
carve. `main_main`'s `DispatchOption::Single` arm and
`scripts/single-user-e2e.sh` are the lane's starting surface.

## §4 threaded process model — no fork, no EXEC_BACKEND, sysv shmem N/A — RATIFIED (consolidated 2026-08-18)

**Decision.** pgrust backends are threads in one process sharing one address
space, by design (docs/strategy.md rows "Per-backend state" and item 5:
thread-per-backend + shared catalog caches). Everything in C that exists
only because backends are forked processes is structurally N/A and will not
be ported: fork/EXEC_BACKEND child launch, shmem *attach* (as opposed to
create) paths, SysV main-region shmem, postmaster-death pipes/watches, and
per-process DSM re-mapping.

**Rationale.** Not an omission — the architecture. One address space means
shmem "attach" is a pointer, postmaster death cannot orphan a backend
process, and the SysV-vs-mmap distinction for the main region has no
meaning. Porting these arms would be dead code with no reachable caller.

**Guard sites** (backstops — none is reachable from SQL or config):

| site | behavior if reached |
|---|---|
| `crates/backend/main/main_main/src/lib.rs:343-345` (`DispatchOption::Forkchild`) | panic `DISPATCH_FORKCHILD reached without EXEC_BACKEND` |
| `crates/backend/storage/ipc/pmsignal/src/lib.rs:227,231` | panic `postmaster death monitoring/signaling is not ported (threaded-model owner: postmaster)` |
| `crates/backend/storage/ipc/waiteventset/src/epoll.rs:45,137`, `kqueue.rs:183` | `unreachable!("postmaster death watch is not ported")`; latch-side events inert by design (`latch/src/lib.rs:196`) |
| `crates/backend/utils/hash/dynahash/src/lib.rs:265-268` (`HASH_ATTACH`) | panic; shmem hash tables are created once and shared by pointer (`lock/src/shared.rs:47`) |
| `crates/backend/access/session/src/lib.rs:416-417` (`GetSessionDsmHandle`) | panic; parallel workers share the address space, session DSM is meaningless |
| `crates/backend/storage/ipc/ipci/src/lib.rs:19` + `dsm_core/src/dsm.rs:493` | comment-fenced: `AttachSharedMemoryStructs` / EXEC_BACKEND re-mapping have no thread-model counterpart |
| `crates/backend/storage/lmgr/lmgr_proc/src/lib.rs:949` | C's fork()ed-child guard, N/A comment |

**User-visible behavior.** None on any supported path. `shared_memory_type`
still lists `sysv`/`mmap` (guc_tables/src/tables.rs:506-508,1199) but the
value is inert — no storage code consumes it; the main region is process
memory. (If a lint ever demands parity, hiding `sysv` is cosmetic, not a
behavior change.)

**Re-open trigger.** Abandoning the threaded architecture — i.e. never; this
section exists so audits stop re-flagging the arms as debt.

## §5 io_method limited to sync/worker — RATIFIED (consolidated 2026-08-18)

**Decision.** The `io_method` GUC accepts only `sync` and `worker`.
`io_uring` is deliberately unlisted until the io_uring lane lands (it is a
chartered lane — docs/strategy.md item 11, M4–M6; docs/beat-postgres.md —
not a permanent carve).

**Rationale.** C 18 compile-gates unavailable methods out of
`io_method_options`; pgrust does the same by not listing them
(aio_core/src/lib.rs:57 comment: "io_uring stays unlisted until inc-2").
Listing an unimplemented method would trade a clean GUC error for a
boot-time panic. The archil-neon incident (aio_uring/src/lib.rs:6-9) pins
why the route must stay io_method-gated.

**Guard sites.**

| site | role |
|---|---|
| `crates/backend/storage/aio/aio_core/src/lib.rs:56-59` (`IO_METHOD_OPTIONS`) | closed option set: sync, worker |
| `crates/backend/storage/aio/aio_core/src/lib.rs:324-343` (`check_io_method`) | GUC check backstop: rejects any listed-but-unported value with errdetail `io_method="<name>" is not yet supported by pgrust; use "sync" or "worker".` |
| `crates/backend/storage/aio/aio_core/src/lib.rs:85-90` (`pgaio_method_kind`) | invariant tripwire panic if the pins are bypassed |

**User-visible behavior.** `SET io_method = 'io_uring'` / postgresql.conf →
`invalid value for parameter "io_method": "io_uring"` with the enum HINT
listing `sync, worker` (the unlisted value never reaches the check hook).
Note: boot default diverges — pgrust `sync` vs C 18 `worker` (flagged
in-file; part of this ruling until the worker default is revisited).

**Re-open trigger.** The io_uring increment (inc-2) landing — this section
then shrinks to the sync/worker/io_uring set of that day.

## §6 min_dynamic_shared_memory pinned to 0 — RATIFIED (consolidated 2026-08-18)

**Decision.** `min_dynamic_shared_memory` accepts only 0 (disabled). The DSM
*main region* (C dsm.c `dsm_shmem_init` + utils/mmgr/freepage.c) is unported.
Ordinary DSM segments (handle-keyed, used by parallel query) are fully live;
only the preallocated main region is fenced.

**Rationale.** freepage.c exists to sub-allocate a fixed preallocated
region; in the threaded model DSM is already cheap process memory, so the
region buys nothing until a workload proves otherwise. Rejecting at GUC
check turns a postgresql.conf edit into a clean startup error instead of a
boot-time panic.

**Guard sites.**

| site | role |
|---|---|
| `crates/backend/storage/ipc/dsm_core/src/lib.rs:13-29` (`check_min_dynamic_shared_memory_hook`) | the pin: nonzero rejected with errdetail `min_dynamic_shared_memory is not yet supported by pgrust; only 0 (disabled) is accepted.` |
| `crates/backend/storage/ipc/dsm_core/src/dsm.rs:256-267` (`main_region_unported`) | invariant tripwire panic — unreachable while the pin holds |
| `crates/backend/storage/ipc/ipci/src/lib.rs:169-177` (`ResetShmemAfterCrash`) | crash-restart backstop panic if a main region ever exists |

**User-visible behavior.** Setting it nonzero → `invalid value for parameter
"min_dynamic_shared_memory": <n>` + the errdetail above; server refuses to
start (postgresql.conf) or SET fails. Default 0 matches C's default, so
stock configs are unaffected.

**Re-open trigger.** A measured workload win from preallocated DSM, or the
main region becoming load-bearing for some ported feature.

## §7 COPY parquet is read-only (COPY TO refused) — RATIFIED (consolidated 2026-08-18)

**Decision.** `COPY ... FROM ... WITH (FORMAT 'parquet')` (pg_parquet's
spelling) is an ingest-only surface; the parquet *writer* is not planned.
This is a pgrust-only feature with no C analog — the ruling bounds the
pgrust feature, not C parity.

**Rationale.** The reader exists to load analytics data into pgrcolumnar
fast; a writer is a separate product (schema mapping, codecs, statistics)
with export tools already covering the need.

**Guard sites.**

| site | user-visible behavior |
|---|---|
| `crates/backend/commands/copy/src/lib.rs:804-810` | `ERROR: 0A000: COPY TO with parquet format is not supported` |

Adjacent, same surface, not part of the carve (open gaps of the reader):
COPY FROM STDIN with parquet (fromparquet.rs:278) and non-UTF8 databases
(fromparquet.rs:374) are tracked gaps, and the 16 parquet_read `unsupported`
arms (encrypted files, Int96/Boolean/FLBA, missing codecs) are reader debt.

**Re-open trigger.** A user-demanded export path that psql `\copy` + client
tooling cannot serve; would be chartered as its own lane.

## §8 wasm32-wasip1 platform arms — RATIFIED (consolidated 2026-08-18)

**Decision.** The wasm32-wasip1 target is compile-clean (788/789 crates,
`scripts/wasm-build.sh` ratchet gate, ledger: docs/design/wasm-crate-ledger.md)
but functionally reduced by design: the wasm arms are platform carves, not
debt. Fenced per the ledger: pgrcolumnar ZSTD (no zstd C lib), mmap SegMap
(heap materialization instead), timeouts (no SIGALRM/threads), WaitEventSet
(time-only), signals (no-op bridge), OpenSSL (`be_secure_openssl` excluded;
`ssl` feature + target gate).

**Guard sites** (representative; the ledger is the full inventory):

| site | behavior |
|---|---|
| `crates/backend/access/pgrcolumnar/src/reader.rs:1054-1057` | panic `pgrcolumnar: ZSTD frames are not supported on wasm32-wasip1` (read of a ZSTD-compressed part) |
| `crates/backend/access/pgrcolumnar/src/writer.rs:264-265` | panic `pgrcolumnar: ZSTD codec is not supported on wasm32-wasip1`; Zstd/Auto codec choice degrades to LZ4 at write per the ledger |
| `crates/backend/storage/ipc/waiteventset/src/wasm_stub.rs` | time-only backend; infinite latch waits report a clean ERROR |
| `crates/backend/main/main_main/src/lib.rs` (`get_user_name_or_exit`, wasm arm) | identity from `USER` env or exit(1) |

**User-visible behavior.** Native builds: none (all arms are `cfg`-gated).
Wasm builds: the behaviors above; a pure-Rust zstd decoder (ruzstd) is the
documented out if the ZSTD arm ever matters.

**Re-open trigger.** Wasm graduating from boot-demo (P5) to supported
deployment target; each ledger row then becomes a chartered lane.

## §9 win32 / eventlog — RATIFIED (consolidated 2026-08-18)

**Decision.** Windows is not a target; the win32-only surfaces are not
ported. The only user-reachable shard is `log_destination = 'eventlog'`,
which pgrust rejects exactly as *non-Windows C* does.

**Guard sites.**

| site | behavior |
|---|---|
| `crates/backend/utils/error/elog/src/config.rs:265-287` (`check_log_destination`) | `eventlog` not in the accepted set → `invalid value for parameter "log_destination": "eventlog"` + detail `Unrecognized key word: "eventlog".` — byte-identical to C built without WIN32 |
| `crates/backend/utils/error/elog/src/report.rs:580` | write-side comment: win32 eventlog destination intentionally not ported |

The GUC's long_desc still names "eventlog" among platform-dependent values
(guc_tables/src/tables.rs:1111) — same text as C, correct as-is.

**Re-open trigger.** A Windows port being chartered (nothing on any roadmap).

## §10 `--describe-config` CLI — RATIFIED (consolidated 2026-08-18)

**Decision.** `postgres --describe-config` (GucInfoMain,
src/backend/utils/misc/help_config.c) is not ported. It is a rarely-used
admin-tooling dump of the GUC tables; `pg_settings` serves the same data on
a live server.

**Guard sites.**

| site | behavior |
|---|---|
| `crates/backend/main/main_main/src/lib.rs:346-348` (`DispatchOption::DescribeConfig`) | panic `GucInfoMain unported: unit backend-utils-misc-help-config` |

**User-visible behavior — known guard debt.** Today the flag panics with a
Rust backtrace, and `--help` still advertises the flag
(main_main/src/lib.rs:455). Under this document's own rule (guards are
product surface, never panics on user-reachable paths) the arm is owed a
clean stderr refusal + exit 1 — the panic-inventory (bucket B, first polish
item) already flags it. Ratifying the carve does not ratify the panic;
convert on the next main_main touch.

**Re-open trigger.** External tooling found depending on the flag's exact
output (pgAdmin dropped it years ago), or the standalone-artifact goal of §1
reopening.

## §11 UTF-8-only server encodings — RATIFIED 2026-08-18

**RULING: Option A — UTF-8-only server encodings — RATIFIED 2026-08-18 by
Jordyn.**

**Decision.** Only `UTF8` and `SQL_ASCII` are accepted server encodings.
SQL_ASCII stays creatable per the memo's recommendation (spelling (a)):
ASCII ⊆ UTF-8, and the existing per-query non-ASCII gate
(`non_utf8_query_error`) remains SQL_ASCII's enforcement. All other non-UTF8
server encodings are rejected at the boundary — at CREATE DATABASE and at
connection time — instead of failing per-feature after acceptance. Client
encodings are unaffected: the full conversion stack
(`crates/backend/utils/mb/{mbutils,conv}`) serves every client encoding
against a UTF8 database. Decision memo (facts, options, costs):
notes/design/non-utf8-encodings-question.md.

**Guard sites.**

| site | behavior |
|---|---|
| `crates/backend/commands/dbcommands/src/createdb.rs:36-70` (`server_encoding_gate`, applied after encoding validation at :581) | `CREATE DATABASE ... ENCODING <other>` → `ERROR 0A000: server encoding "<NAME>" is not supported by pgrust; only "UTF8" and "SQL_ASCII" server encodings are accepted (UTF-8-only carve, docs/design/carve-ratifications.md)` + `HINT: Restore non-UTF8 dumps into a UTF8 database; client_encoding conversion is supported.` |
| `crates/backend/utils/init/postinit/src/lib.rs:154-183` (`unsupported_database_encoding_error` / `check_database_encoding_supported`, applied in `CheckMyDatabase` at :250, before `SetDatabaseEncoding`) | connecting to a database whose pg_database.encoding is neither UTF8 nor SQL_ASCII (e.g. a LATIN1 database created offline by C's initdb/createdb — the §1 initdb-via-C carve) → `FATAL 0A000: database "<name>" has server encoding "<NAME>", which is not supported by pgrust; only "UTF8" and "SQL_ASCII" server encodings are accepted (UTF-8-only carve, docs/design/carve-ratifications.md)` + the same hint. Per-database refusal: the cluster boots and its UTF8/SQL_ASCII databases stay fully usable. |
| `crates/backend/tcop/postgres/src/lib.rs` (`non_utf8_query_error`; fired from main_loop.rs simple-query and extended_query.rs Parse) | STAYS — SQL_ASCII's enforcement: non-UTF8 query text → `ERROR 0A000: query strings with non-ASCII characters are not supported yet in databases with encoding "<NAME>"` + `HINT: Use a database with encoding "UTF8".` For non-UTF8 multibyte encodings it is now a backstop behind the boundary gates. |
| `crates/backend/parser/scan_fgram/src/lex.rs` (`utf8_pin`, after C's `pg_verifymbstr` pass in `stop_string`) | SQL_ASCII enforcement for the one path that assembles non-UTF8 bytes out of pure-ASCII query text: an `E''` literal whose octal/hex escapes build bytes no `&str` can carry (`E'\xc3\x28'`, `E'\200'`) → the same `ERROR 0A000: query strings with non-ASCII characters ...` + hint, with a cursor at the literal. C keeps the raw bytes (scan.l:619 `pg_verifymbstr` is NUL-only under SQL_ASCII); pgrust used to trip gram_core's `from_utf8` assert (audit finding catalog-2). Escape-built valid UTF-8 and `\u` escapes are unaffected (`\u` above U+007F draws C's own 0A000 "conversion between UTF8 and SQL_ASCII is not supported"). |
| `crates/backend/commands/extension/src/lib.rs` (`non_utf8_text_error`; fired from script.rs `script_text` and control.rs `read_extension_control_file_bytes`) | an extension script whose bytes pass `pg_verify_mbstr`/`pg_any_to_server` but are not UTF-8 (a SQL_ASCII database, where C executes the raw high-bit bytes; also a script with an explicit `encoding = LATIN1`-style control setting installed into a SQL_ASCII database, where C cannot convert and runs the bytes as-is) → the same `ERROR 0A000` + HINT as the tcop gate, raised before the script runs, no session state left behind (extension-1); and `pg_extension_update_paths(<name>)` whose name bytes (a bind parameter — `pg_client_to_server` validates only NUL in SQL_ASCII) are not UTF-8 *and* name a control file that exists on disk (a name pgrust cannot spell). A non-UTF-8 name with no such file is C's own `0A000 extension "…" is not available` + HINT, and non-UTF-8 catalog bytes (`extname`, `extversion` hacked through a bind parameter) take C's paths — `pg_describe_object` succeeds, `ALTER EXTENSION UPDATE` reports C's `22023 … has no update path …` — and every one of these error messages (`invalid extension name`, `… is not available`, `… has no update path …`) carries C's exact name/version bytes on the wire (`PgError::error_raw_message`, the byte-string primary message elog already sends for C's `%s`/`%c` of raw bytes). The one rendering that stays different is `pg_describe_object`'s *result*: `get_extension_name` returns a `PgString`, so the description of a non-UTF-8 `extname` is `extension ea_b\uFFFD` where C emits the raw bytes — the catalog-NameData-as-text wall shared with objectaddress/lsyscache (`name_from_datum`, `name_to_pgstring`), not this crate's. In a UTF8 database the script goes through `pg_verify_mbstr`/conversion exactly as in C. |
| `crates/backend/commands/comment/src/lib.rs` (`GetComment`, the `pg_description`/`pg_shdescription` reader behind `CREATE TABLE (LIKE ... INCLUDING COMMENTS)` and the ALTER TABLE rewrite paths) | SQL_ASCII enforcement for the one path that reads non-UTF-8 bytes back OUT of the catalog (a comment planted through `UPDATE pg_description SET description = convert_from('\x...','SQL_ASCII')`; `COMMENT ON ... IS E'\x80'` is already stopped by the scanner's `utf8_pin`): C (comment.c:449 `TextDatumGetCString`) copies the raw bytes into the new comment; pgrust answers `ERROR 0A000: comments containing non-UTF-8 bytes are not supported yet in databases with encoding "<NAME>"` + the same HINT (landed by PR #1786, audit-18.6 b131; the panic it replaced was audit finding comment-18ce). Carrying the bytes would require a non-`&str` `CommentStmt.comment` carrier through the parse tree — the memo §3 Option-B cost, not re-opened. |
| `crates/backend/utils/adt/like/src/lib.rs` (`mb_matchtext_unported`) | retained backstop (unreachable: SQL_ASCII is single-byte and takes the ported SB path) |
| `crates/backend/commands/copy/src/fromparquet.rs:367-377` | retained: parquet text columns require a UTF8 database — still reachable via SQL_ASCII |

**User-visible behavior / accepted divergences from C** (memo §2): restoring
a non-UTF8 dump requires creating the target database as UTF8 and restoring
into it — pg_dump's `SET client_encoding` makes the data transcode
losslessly into UTF-8 (proved end-to-end by
scripts/utf8-only-ratification-e2e.sh, dump-restore leg). The on-disk
byte-compatibility claim of §1 narrows to UTF8/SQL_ASCII clusters. C-locale
initdb (SQL_ASCII cluster) keeps working with the documented query-text
restriction.

**Re-open trigger.** A priced Option-B charter (the memo §3 XL non-UTF8
engine lane), driven by concrete demand that client-side conversion cannot
serve.

**Addendum 2026-09-16 — UTF8 is the ONLY accepted server encoding (owner
ruling, bug campaign 2026-09-13).** The owner ruled "pgrust should stay
utf-8 only" and "change it so you can't create any non utf8 database":
SQL_ASCII is no longer creatable or connectable. The 2026-08-18 spelling (a)
above kept SQL_ASCII creatable because its query text was gated to ASCII;
in practice SQL_ASCII was the loophole every non-UTF-8 deferred campaign row
used (`convert_from(..., 'SQL_ASCII')` and bind parameters smuggle raw bytes
into identifiers, defaults, trigger names, messages and fdw pushdown —
the 17 `deferred:non-UTF8-encodings-unported` / SQL_ASCII / UTF-8-only rows
of the campaign ledger, now `not-significant (utf8-only ruling 2026-09-16)`).
Guard sites, same message shape, reworded: `server_encoding_gate` (createdb.rs)
→ `ERROR 0A000: server encoding "<NAME>" is not supported by pgrust; only the
"UTF8" server encoding is accepted (UTF-8-only carve,
docs/design/carve-ratifications.md)` for every non-UTF8 encoding, SQL_ASCII
and the numeric code 0 included, and now also for an encoding *inherited*
from a non-UTF8 TEMPLATE (a C initdb without `-E UTF8` leaves SQL_ASCII
templates); `unsupported_database_encoding_error` (postinit) → `FATAL 0A000:
database "<name>" has server encoding "<NAME>", which is not supported by
pgrust; only the "UTF8" server encoding is accepted (...)` for SQL_ASCII
databases too, so a data directory whose template0/template1 are SQL_ASCII
(C initdb under the C locale without `--encoding=UTF8`) boots but refuses
every connection to those templates and to anything cloned from them; the
HINT is unchanged. initdb is C's (§1) and the harnesses run it with
`--encoding=UTF8` (scripts/boot-smoke.sh, every *-e2e.sh). The per-query
`non_utf8_query_error`, `utf8_pin`, `non_utf8_text_error` and `GetComment`
backstops in the table above stay as defense in depth but are no longer
reachable through a creatable database. Pin: scripts/utf8-only-ratification-e2e.sh
(SQL_ASCII by name / by code / by TEMPLATE refused; C-created SQL_ASCII and
LATIN1 databases FATAL at connect; UTF8 unaffected; dump-restore leg). The
e2e fixtures and scripts that created SQL_ASCII databases were cut to the
refusal (their expected output is the 0A000) or dropped when SQL_ASCII was
their only purpose.

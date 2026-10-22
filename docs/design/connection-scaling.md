# Connection scaling without a pooler — plan

Status: **campaign complete through wave 3a** (2026-08-13; measurements in
notes/connection-scaling-measurements.md). Shipped: D1 admission gate, D6
connection queue, D3.0 census tooling, D3.1 bounded L1 caches, D3.2 shared
immutable L2 (+ wave-3a init-file→L2 routing), D3.5 snapshot-array diet,
D3.4 idle passivation; all PoC env knobs promoted to GUCs (see table below).
Skipped with evidence: D2 (by measurement — parked threads are cheap),
D3.3 (by census — plancache is 4KB on the workload). D4 Linux validation
ran 2026-08-13: **mechanisms PASS, memory numbers DO NOT TRANSFER — Linux
idle floor is 6MB/conn RSS (~4.3x the macOS fp numbers below), open issue;
plus an unresolved soak RSS-creep flag.** See the D4 section at the end of
notes/connection-scaling-measurements.md before quoting any MB/conn figure.

**Success-bar verdict (honest):** warmed **3.46** / passivated **2.99**
fp MB/conn against the ≤~1MB bar — the reclaimable cache estate is **fully
reclaimed** (passivation lands ~0.15-0.2MB above the idle-bootstrap floor),
but the bar itself is NOT hit: the remaining ~2.8-3.0MB/conn is the
bootstrap floor, so reaching ≤1MB requires floor work — thread-stack dirty
pages (madvise discipline), bootstrap allocator churn, and the fixed
per-backend arenas — listed as future work, not in this campaign.

**Wave 4 (floor work) addendum — 2026-08-13, notes §Wave 4:** the floor
census FALSIFIED the stack hypothesis (a ready backend has dirtied only
**76KB** of stack, not ~1.5MB) and pinned the floor on **allocator page
geometry** (mimalloc per-thread heaps: one 64KB committed page per active
size class, live-sparse after bootstrap churn). Shipped: floor-census
instrumentation (stack mincore scan + allocator stats in `pgrust: memctx`),
stack release at passivation (Linux MADV_DONTNEED / macOS
MADV_FREE_REUSABLE+REUSE; GUC `idle_passivate_stack`, default on), PgStat
Pending 74KB→1.7KB, oper-lookup 72KB→~4KB (+ dropped at passivation), L2
shell diet round 2 (RdAmCacheGin 516B + SpGistCache 128B boxed; −87.5KB/conn
over 139 boot entries; also fixed a missed spgist-amcache invalidation).
Live bytes −158KB/conn (census-exact); phys_footprint moved only ~0.06MB
passivated (2.81→2.75 mean, interleaved A/B — inside noise) because freed
bytes return to committed 64KB thread-heap pages. **Bar still NOT hit**;
the remaining ~2.4-2.8MB/conn is named: allocator page estate, attacked
next by commit-on-demand / arena-izing session-lifetime boxes, not by more
cache work.

| Milestone | Status | Result (gate evidence in notes/) |
|---|---|---|
| Phase 0 measurement | DONE | warmed 3.2-4.4 vs C 8.6-14.1 MB/conn; connect ~1ms both; idle-conn tps flat both |
| D1 admission gate | SHIPPED | `max_active_queries`; queue-before-snapshot/locks; suites green |
| D6 connection queue | SHIPPED | `connection_queue_size`/`_timeout`; burst e2e (140 conns vs max 50 + queue 100) zero errors, all served; timeout/hangup/shutdown legs green |
| D3.0 census | DONE | warmed 3.2MB = caches 1.1MB + SnapMgr 297KB + floor 1.4-1.6MB |
| D3.1 L1 caps | SHIPPED | `catcache_size_limit`=2048 / `relcache_size_limit`=512; many-object tail bounded; perf 1.000/0.997; trims at the idle point since D3652 P8 |
| D3.2 shared L2 | SHIPPED | `shared_catalog_cache`; +shell diet 4.70→3.92 fp; DDL stressor found+fixed 3 coherence holes |
| wave 3a init-file→L2 | SHIPPED | 139/139 boot entries as shells; 3.55→3.46 fp; −78KB/conn live context |
| D3.5 snapshot diet | SHIPPED | SnapMgr 297KB→1KB (2.7MB→1KB at mc=5000); deadlock arena lazy; perf 0.999 |
| D3.4 idle passivation | SHIPPED | `idle_passivate_timeout`=60s; warmed 3.46→passivated 2.99 fp; reactivation ~4ms worst-probe |
| D2 idle-socket reactor | **SKIPPED by measurement** | Phase 0: parked threads cheap, idle tps flat — nothing to harvest at ≤10k conns |
| D3.3 shared plan store | **SKIPPED by census** | plancache = 1 source / 4KB on the workload; revisit only with plan-heavy evidence |
| D4 5k-conn verification | **PENDING (fleet)** | needs Linux fleet legs at 2k/5k idle + 64 active |
| D5 ceilings/defaults | DONE (measure) | ceilings + stack reservation documented; max_connections default raise deferred until D4 |

GUCs as landed (env harness overrides still win when set):
`max_active_queries` (SIGHUP, 0=off) · `connection_queue_size` (POSTMASTER,
0=off) · `connection_queue_timeout` (SIGHUP, 5000ms) · `catcache_size_limit`
(SIGHUP, 2048, 0=off) · `relcache_size_limit` (SIGHUP, 512, 0=off) ·
`shared_catalog_cache` (POSTMASTER, on; init-file routing folded in) ·
`idle_passivate_timeout` (SIGHUP, 60s, 0=off).

Original plan ruling below, kept for provenance. Ruling: pursue "option 3"
— make connections cheap enough that multiplexing is unnecessary, plus a
minimal admission gate. Explicitly rejected: a built-in transaction-mode
pooler (relocates pgbouncer's session-semantics weirdness, doesn't remove
it). Deferred, not rejected: full session/worker M:N (externalized session
state; the bgjobs GUC-overlay pattern proves the shape in-tree) — it is the
only road to ~100k connections, and nothing below forecloses it.

## 0. The three problems a pooler solves, and where pgrust stands

| Pooler job | C Postgres | pgrust today | This plan |
|---|---|---|---|
| Connection establishment | fork + init, ~ms | thread spawn (measuring) | D5 verify; expected already-solved |
| Idle-connection memory | 5–50MB/backend private caches | thread_local caches — same growth (measuring) | **D3: shared caches** (the centerpiece) |
| Active-query admission | pooler caps in-flight work | none | **D1: admission gate** |

Phase 0 — **MEASURED** (notes/connection-scaling-measurements.md, harness in
scripts/; Mac14,7, pgrust release vs C 18.4, identical config):

| Metric | pgrust | C 18.4 |
|---|---|---|
| Idle MB/conn | 0.41 RSS / 2.8 footprint | ~0.6 marginal |
| **Warmed MB/conn** | **3.2 RSS / 4.4 footprint** | **8.6–14.1** |
| Connect (ms) | 0.8–1.2 | 1.0 |
| tps -S c8, 0 vs 400 idle | 173k → 177k (flat) | 200k → 198k (flat) |
| Per-user ceiling (macOS) | ~2,048 threads | ~1,333 procs |

Implications, baked into the milestones below: **connection establishment
needs no work** (~1ms both engines — no fork-cost for a pooler to harvest);
**idle-connection tps impact needs no work at this scale** (dense procarray +
GetSnapshotDataReuse ported and visibly working; D4 still verifies at 2k/5k
on Linux); the whole remaining pooler case is **warmed cache memory** —
pgrust is already 2–3× cheaper than C (3.2–4.4 vs 9–14 MB/conn) but 500
warmed connections still cost ~1.6–2.2GB against the 500MB success bar, so
**D3 must deliver ~4×**, which the D3.1 L1 caps alone plausibly reach.

**Success criterion (the "pooler unnecessary" bar):** 500 warmed idle
connections ≤ ~500MB total (≤ ~1MB each); connect p95 < 1ms; 5,000
connections configurable and stable; zero measurable tps impact from idle
connections; under 4× CPU oversubscription of active queries, p99 latency
degrades gracefully instead of collapsing. Hit that and pgbouncer has no
remaining job at any workload under ~10k connections.

## D1 — Admission gate (small, independent, ships first)

`max_active_queries` GUC (0 = off): a counting semaphore acquired at the
first statement of a transaction, released at transaction end. Excess
concurrent transactions queue FIFO for the semaphore instead of thrashing
CPU/locks.

**Grain amendment (2026-08-13, post-validation):** the original design
below gated every statement and released the slot between the statements
of an open transaction. The pooler validation (validate/connscale,
notes/connscale-validation-vs-pgbouncer.md §6a) proved that unsafe for RW
work: a multi-statement transaction holds row locks between statements, so
its next statement parked behind the admitted statements blocked on its
own locks — an undetectable, permanent freeze (tpcb c=128 at gate=32 froze
in seconds). The gate now admits at TRANSACTION grain: the slot is taken
at a transaction's first statement and held to transaction end (commit,
rollback, pipeline Sync, error recovery, or backend exit), and statements
inside an admitted transaction never park — a parked waiter therefore
holds no locks and no snapshot, by construction. This matches pgbouncer
transaction-pooling semantics, including the corollary that an
idle-in-transaction session keeps its slot (idle_in_transaction_
session_timeout is the mitigation, as with a pinned pooler connection).
Regression e2e: scripts/maq-txn-gate-e2e.sh.

- **Acquire point**: top of statement execution in tcop (exec_simple_query /
  exec_execute_message), BEFORE snapshot acquisition and lock-taking — a
  queued transaction must not hold a snapshot (vacuum horizon) or locks
  (deadlock surface). Reentrancy counter so SPI/nested execution never
  double-acquires; statements after the first in an admitted transaction
  bypass the gate (the session already holds the slot). A live transaction
  that reaches the gate slot-less (gate armed mid-transaction by SIGHUP, or
  an exemption lapsed mid-transaction) runs uncounted rather than parking —
  it may hold locks; the gate reconverges at its next transaction.
- **Exemptions**: superuser reserved slots (mirroring
  superuser_reserved_connections), replication/walsender, and anything not
  running through the executor (utility fast-paths measured first).
- **Observability**: wait event + pg_stat counter for queue depth/time.
- **Gate**: pgbench -c 500 -j 8 on a 8-core box, gated at 16 vs ungated:
  p99 improves, no starvation (fairness = FIFO), tps not worse; isolation
  tests green (the gate must never reorder around lock waits it didn't
  cause).

Risk: low. This is admission control, not multiplexing — 1 connection stays
1 session with 1 identity; no visible semantics change.

## D2 — Idle-socket reactor (SKIPPED — Phase 0 said so)

**Final status: skipped by measurement.** Phase 0 measured parked threads as
cheap (0.41 RSS / 2.8 fp MB idle, tps flat at 400 parked) — exactly the
skip condition below. Remains the first brick of the deferred M:N road if
>10k connections ever becomes a requirement.

Thread-per-connection means an idle connection = a parked thread. If Phase 0
shows parked threads are cheap (expected: kernel task + committed stack
pages, likely ~100KB), **skip D2 entirely** — epoll parking buys nothing
when the thread must exist anyway. Build it only if idle-thread cost or
scheduler pressure measures as a real limit (likely only relevant >10k
conns), at which point it is also the first brick of the deferred M:N road:
idle sockets parked on the existing io-pool runtime, thread handed back,
session resumed on data arrival. Costed separately if triggered.

## D3 — Shared caches (the centerpiece)

The reason idle connections are expensive is per-connection cache growth:
relcache, catcache (syscache), plancache, typcache — all `thread_local` in
pgrust (inherited from C's process-private world). Threads can share;
processes never could. This is pgrust's structural advantage — spend it.

### Design: bounded L1 + shared immutable L2

Two-layer, chosen over a single fully-shared cache to keep the hot path
untouched:

- **L1 (per-thread, BOUNDED)**: today's thread_local caches gain a hard
  entry cap (e.g. relcache 512, catcache per-cache caps, sized from Phase 0
  data) with LRU eviction. A cache hit is exactly today's code — no new
  locking on the hot path. The cap converts per-connection memory from
  unbounded to a configured ceiling.
- **L2 (process-global, shared, immutable entries)**: on L1 miss, consult a
  shared map keyed by (cache, key, **catalog generation**); entries are
  immutable `Arc`s built once and shared by all threads. L1 eviction is
  free (drop the Arc). On L2 miss, one thread builds the entry (per-key
  build lock so 500 connections warming simultaneously build each entry
  once — this also fixes the thundering-herd cold-start that poolers
  accidentally mitigate today).
- **Invalidation**: sinval traffic keeps working exactly as today for L1
  (each thread processes its queue and drops local entries). L2 adds a
  monotonic generation per invalidation domain: sinval messages bump the
  generation; L2 lookups carry the reader's current generation, so stale
  entries are unreachable the instant the bump lands and are reclaimed
  lazily (epoch/Arc refcount — no reader locks). Uncommitted DDL never
  enters L2: a session that has modified a catalog reads through its own
  L1-with-uncommitted-overlay until commit (the classic
  shared-committed/private-dirty split; DDL sessions temporarily bypass L2
  for the affected keys — correctness first, DDL performance unchanged).
- **Plan cache**: separate sub-milestone. Prepared statements stay
  per-session by definition; what shares is the **generic plan** body keyed
  by (query fingerprint, search_path, relevant GUC fingerprint, catalog
  generation). Sessions hold Arcs into the shared plan store. Custom plans
  stay session-local and bounded.

### D3.0 census verdict (2026-08-12) — revises the emphasis below

Measured split of the warmed 3.2MB: caches ≈ **1.1MB** (catcache 285KB,
relcache ~0.7MB incl. heap entries, small others), **SnapMgr snapshot
arrays ≈ 297KB** (xip/subxip sized by max_connections — C-parity shape,
~150MB across 500 conns), remainder ≈ 1.4–1.6MB of stack + allocator
retention. Consequence: D3.2's sharing caps out around ~0.9MB/conn of
savings; reaching the ≤1MB bar ALSO requires (new) **D3.5: snapshot-array
diet** — lazily size xip arrays to observed connection count or share
snapshot buffers, instead of max_connections-sized per-thread arrays — and
makes **D3.4 (arena/stack trim)** co-equal with sharing rather than
cleanup. D3.1 shipped with defaults (catcache 2048 tuples /
relcache 512 entries, env-overridable): unbounded growth is now capped
(800-table session settles at the caps), full suites + low-cap stress
green, perf flat.

### Milestones

- **D3.1 relcache+catcache L1 caps** (memory ceiling with zero sharing):
  the cheapest 80% of the memory win. Gate: warmed-100-connections memory
  drops to ~cap×entry-size×conns; pgbench regression < 2%; full regression
  suites green (cache-pressure edge: run with caps set absurdly low, e.g.
  16, to shake out eviction-order bugs — entries pinned by open cursors
  must be evict-exempt, mirroring C's refcounted relcache pins).
  **D3652 P8 (2026-10-10): the trims run at the idle point.** A session
  (PostgresMain) no longer evicts on each catcache insertion or trims the
  relcache at each transaction end; ready_state trims both to the caps
  right after the statement's ReadyForQuery, before the per-statement
  memory release (`passivate::trim_caches_at_idle`). Same caps, same
  eviction order and exemptions (pinned / listed / dead catcache entries;
  nailed, held and subid-state relcache entries), L2 path unchanged; the
  relcache trim still waits for the transaction end (first idle point
  outside a transaction block). Background workers and WAL senders keep
  the insert / commit trims. Consequence for the memory redesign: a session's L1 is at
  or under the caps whenever it is idle, but grows past them for the
  length of one statement (transient growth, accepted by the user): a
  4000-partition cold count no longer evicts and re-reads its own
  catcache working set mid-statement, and both trims leave the client
  latency.
- **D3.2 shared L2 for catcache/relcache** + generation invalidation.
  Gate: warmed MB/conn ≤ target; 500-connection warm-start time (cold
  cache → all warmed) improves vs today (herd fix); DDL/sinval stress
  (concurrent DDL + 100 querying threads, the existing inval tests +
  a new stressor) green; pgbench flat.
  **SHIPPED** (notes §D3.2): process-global generation-keyed map of
  immutable entry bodies (Arc), per-key build gates, PGRUST_L2_CACHE=0
  kill switch. Catcache tuples share their image buffers outright (L1
  entries alias the Arc). Relcache required the census-mandated split:
  a shareable immutable core (pg_class row, pg_attribute array,
  constraint sources, index arrays — 69-75% of entry bytes after the
  rd_options shell diet) + a thread-local shell holding all
  thread-affine state (smgr, subids, pgstat, amcaches, supportinfo).
  Generations: bump-before-queue on the sender, view-sync where sinval
  processing drops L1 — staleness-until-Accept preserved exactly.
  Uncommitted DDL bypasses L2 wholesale until commit (coarse overlay);
  parallel workers and historic snapshots bypass too. The new stressor
  (scripts/l2-ddl-stress-e2e.sh) found and fixed three latent
  coherence holes (startup-window inval discards, registration-skipped
  bumps, fastpath-lock/sinval memory ordering — the last a genuine
  C-divergence that could lose an invalidation past a freshly granted
  lock even without L2). Measured (Mac14,7, release, notes §D3.2):
  pgbench flat (simple 0.99x, prepared 1.01x, within the 2% gate);
  100-conn warm-start vs a 200-table schema improves ~4-20%; the
  warmed-100 harness footprint moved only ~0.05MB/conn from SHARING —
  the shell-audit diet (boxing rd_options) delivered ~0.8MB/conn
  (4.7 -> 3.9 fp) — because most relcache entries bulk-load from the
  relcache INIT FILE per backend and bypass the L2 (routing init-file
  entries through shared cores is the headline deferred item), and the
  rest of the floor is SnapMgr + stacks + allocator retention →
  D3.4/D3.5 as the census predicted.
  **Init-file routing SHIPPED (wave 3a, notes §Wave 3a):** loads now
  install-or-adopt shared cores (file bytes + generation-stripe snapshot
  captured under RelCacheInitLock — the unlink-before-bump invariant
  makes the snapshot the file's freshness certificate; publish
  restricted to RelationIdIsInInitFile rels; two-pass load keeps the
  per-backend dirty footprint at one entry). All 139 boot entries become
  shells; measured −69KB/conn CacheMemoryContext, −78KB/conn live
  context bytes, warmed-100 3.55 → 3.46 fp — the honest post-diet
  shareable remainder, not the pre-diet ~0.4-0.5MB estimate.
- **D3.3 shared generic-plan store. SKIPPED by census** — the D3.0 census
  measured plancache at 1 saved source / 4KB on the warmed workload; there
  is no per-connection plan estate to share at this scale. Revisit only
  with evidence from a plan-heavy workload (hundreds of prepared
  statements per session). Original gate kept for that day:
  prepared-statement pgbench
  (-M prepared) flat or better; per-connection plan memory bounded;
  plancache invalidation tests green.
- **D3.4 idle passivation + arena trim** (re-scoped per the D3.0 census
  verdict; long-tail sweep folded in where it mattered). **SHIPPED
  (wave 3a, notes §Wave 3a):** GUC `idle_passivate_timeout` (PGC_SIGHUP,
  default 60s, 0=off) — after that long idle outside a transaction the
  backend drops unpinned relcache entries + all catcache tuples +
  generic plan bodies (prepared sources survive), and returns per-thread
  retained allocator memory (parked keeper blocks + mi_collect); first
  query after rebuilds via L2 adopt. Trigger rides the timeout engine
  (one-shot timer per idle period; latch wakes the FeBeWaitSet read
  wait). Measured: warmed 3.46 → passivated **2.99 fp MB/conn** —
  ~0.15-0.2MB above the idle-bootstrap floor, i.e. the reclaimable warm
  estate is fully reclaimed; pgbench with passivation armed-but-idle
  0.993/1.003 (within 1%); reactivation ~4ms on a catalog-heavy probe.
  Stack madvise deliberately skipped (platform-divergent semantics under
  a live thread — see tcop/postgres/src/passivate.rs). Remaining gap to
  the ≤1MB bar is the bootstrap floor (stack dirty pages + startup
  allocator churn + fixed arenas), not caches.

Risk: moderate-high (this is the real engineering). Contained by the
L1-unchanged-hot-path design and by shipping D3.1 (pure bounding, no
concurrency) before any sharing exists.

## D6 — Connection admission queue (queue instead of "too many clients")

Today an over-limit connection gets an immediate FATAL 53300. D6 parks it
in a bounded FIFO instead: backend thread reads the startup packet (so the
superuser-reserved and replication bypasses work as today), attempts the
slot claim, and on "full" waits on a fair condvar — holding a socket + a
parked thread (~0.4MB measured) and NO proc slot/snapshot/locks — until a
slot frees or `connection_queue_timeout` expires (then the same 53300 the
client already handles). `connection_queue_size` bounds the queue (waiters
beyond it fail immediately; also the pre-auth DoS bound); 0 disables = 
today's behavior. Must-handle edges: reap queued clients that hang up
(poll for EOF while parked), wake all waiters on shutdown, FIFO fairness.
Complements D1: D1 queues queries among admitted sessions, D6 queues
admission itself. Gate: burst test — 800 simultaneous connects against
max_connections=500, queue 300: zero errors, all served, orderly latency;
timeout + hangup-reap + shutdown tests; queued clients visible in a stat
view.

**Admission bypass (ruling 2026-09-14).** Michael's rules: (1) at the
ceiling a user goes to the back of the queue until a connection is
available; (2) a superuser skips the queue and gets a connection at once.
Identity is unknown before authentication (no catalog without a PGPROC),
so the split is a pre-auth *claim* read off the startup packet and verified
after login by the stock reserved-slot check. With `band` =
`superuser_reserved_connections + reserved_connections`:
- non-claimers pop a slot only while MORE than `band` are free (arrivals
  and queue waiters alike), otherwise they queue FIFO — an ordinary user is
  never refused at its real ceiling (`max_connections - band`) and never
  consumes the band;
- claimers skip the queue (never divert behind waiters) and pop ANY free
  slot, band included; InitPostgres's reserved check then keeps a superuser
  / `pg_use_reserved_connections` member and refuses anyone else with the
  stock 53300 — a false claim buys nothing but a faster refusal. A claimer
  that finds no free slot at all queues like everyone else.
- claim sources (either suffices): the startup-packet GUC
  `pgrust.admission_bypass=on` (PGC_BACKEND; `PGOPTIONS='-c
  pgrust.admission_bypass=on'` or a startup parameter), or a startup-packet
  `application_name` that case-insensitively prefix-matches an entry of
  `pgrust.admission_bypass_applications` (SIGHUP; default = interactive
  and admin tools only — psql, pgcli, pgAdmin 4, HeidiSQL, TablePlus, dbvis, azdata, OmniDB, SQL Workbench/J, DataGrip, IntelliJ IDEA, DBeaver, Postico —
  never drivers, ORMs, poolers or pg_dump/pg_restore/pgbench; provenance in
  guc_tables `backing.rs`).
- trade-off: a superuser sending neither (a driver connection with a custom
  application_name) is ordinary at the ceiling and waits. `band = 0` or
  `connection_queue_size = 0` keeps today's behavior, claim or not.
Gate: `scripts/release/connqueue-bypass-e2e.sh` (and the earlier
`connqueue-reserved-e2e.sh`).

## D4 — Many-connections scaling verification (PENDING — fleet)

**Final status: pending fleet.** Phase 0 verified flat tps at 400 idle on
the Mac; the dedicated 2k/5k-idle legs need the Linux fleet (macOS thread
ceiling is ~2,048/process) and remain the one open campaign item.

PG18's snapshot scaling (dense xid arrays, `GetSnapshotDataReuse`) is
already ported. Verify it holds at 5k connections: Phase 0 leg 4 at 400
idle, then a dedicated run at 2k/5k idle + 64 active on the fleet
(Linux). Profile and fix hotspots only if found (candidates: ProcArray
add/remove under connection churn, latch/signal fan-out, pgstat). Gate:
tps@64-active flat within 5% from 0 to 5k idle connections.

## D5 — Ceilings, defaults, and establishment latency

- Backend thread stack: confirm spawn-time reserved vs committed;set the
  reservation deliberately (guard page + max_stack_depth interplay);
  document kernel knobs (threads-max, ulimits, somaxconn for accept bursts).
  File descriptors are done: `docs/file-descriptors.md` (one process = one fd
  table, so `kern.maxfilesperproc` / `fs.nr_open` is a whole-server ceiling, and
  on macOS `ulimit -n` cannot raise it). Note for the max_connections bump
  below: descriptors scale with it — 162 sessions reserve 10,962, which is
  already above the macOS default cap of 10,240.
- Raise default max_connections once D3 lands (500? 1000?) with the memory
  math documented.
- Connection establishment: expected already ~free (thread spawn + no
  fork-time cache duplication); Phase 0 measures; if backend-init cost
  (catalog priming) dominates, D3.2's shared L2 also fixes cold-start —
  re-measure after.

## Sequencing & shape

(As planned; the as-run sequence and outcomes are in the Status header.)

Phase 0 (running) → D1 (days; independent win) → D3.1 → D3.2 (the
campaign core) → D3.3 → D4/D5 verification passes → D2 only if numbers
demand. Each milestone gates on the standard suites plus the specific
gates above; sim/DST coverage extends where invalidation ordering is
touched (D3.2's generation bumps are a natural sweep target).

End state: connect in ~100µs, idle connections at ~100KB–1MB warmed,
thousands of connections as a configuration choice rather than an
architecture decision, overload handled by admission instead of collapse —
and no pooler, with no pooler-shaped semantics anywhere.

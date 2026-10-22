# PGO training corpus

Profile-guided optimization needs a workload to train on. This directory holds
that workload, the proof that it is **not** the workload we publish numbers
for, and the machinery that keeps the two apart.

## Why this exists

A PGO binary is shaped by whatever it was profiled running. If the profile run
executes the exact statements a published benchmark later measures, the
published number is partly a measurement of the training, not of the engine —
the compiler has been handed the answer key. That is the ordinary
train-on-test failure, and it is invisible from the outside unless the training
inputs can be read.

So: the training corpus lives in the source tree, under version control; every
training run reads it and nothing else (no downloaded data, no benchmark
tables); and a lint refuses to build if any training statement is a published
measurement statement, or a published analytical statement with its columns
renamed.

## Provenance

One corpus, one training run, two callers:

- the release: `scripts/release/linux-builder.sh` builds the instrumented
  server and runs `scripts/release/pgo-train.sh` on the builder host;
- a local rebuild: `benchmarks/build-pgo.sh` runs the same `pgo-train.sh`.

`pgo-train.sh` expands this directory with `gen-corpus.sh` (which re-runs the
lint over the expansion), trains on a fresh `initdb` cluster, and logs a
`PGO-CORPUS sha256=<hex>` line. The corpus sha256 is the sha256 of the sorted
`sha256sum` listing of `corpus/`, `denylist/`, `gen-corpus.sh` and the two lint
files; it does not depend on where the corpus is expanded, so it names the
corpus revision a binary was trained on. The release builder copies the line
into its own log; `build-pgo.sh` writes it into `build-manifest.txt` next to
the source sha and the profdata and binary hashes.

Where it can be read: this directory ships with the public source tree (the
rest of `benchmarks/` does not), so the corpus a published binary was trained
on is readable in the public tree at the source sha the binary was built from.

## What is here

| path | role |
|---|---|
| `corpus/analytics-pulse.sql` | 88 analytical statements (`J1`…`J88`) over the generated fixture, one per line |
| `corpus/analytics-pulse-engines.tsv` | per-statement execution-engine class (drives the two vectors) |
| `corpus/analytics-pulse-fixture.sql` | the fixture: DDL plus the SQL that generates and loads every row |
| `corpus/oltp-generic.sql` | the transactional statement-class list of record |
| `corpus/oltp-schema.sql` | fixture DDL for the transactional corpus |
| `corpus/QUARANTINE.tsv` | shape ids held out of the emitted vectors, each with its defect |
| `gen-corpus.sh` | expands the corpus into the files a profile run consumes, lints the expansion, prints the corpus sha256 |
| `denylist/analytics-skeletons.sql` | the published single-table analytical measurement vector, stored as its column-blind skeletons (no statement text) — **denied** |
| `denylist/tpch-official.sql` | the published TPC-H vector (22 templates), stored as column-blind skeletons plus the sha256 of each statement's canonical form (no statement text) — **denied**, exact and column-blind |
| `denylist/tpcds-official.sql` | the published TPC-DS vector (99 templates), stored as for TPC-H — **denied**, exact and column-blind |
| `denylist/oltp-official.sql` | statement templates of the published transactional rigs — **denied**, exact form |
| `denylist/EXEMPTIONS.txt` | statements permitted despite a match, each with a reason (currently empty) |
| `lint-training-overlap.sh` / `.awk` | the proof: training ∩ denied = ∅, under both rules |
| `../build-pgo.sh` | a local PGO build from source: instrumented build, `scripts/release/pgo-train.sh`, merge, profile-use build, manifest |

The denylist files are inputs to the lint only. Nothing in the build reads them
as training material.

## The line this draws

**Enforced mechanically, rule 1: no literal overlap.** No statement that a
published benchmark issues may appear in the training corpus, in any binding.
The lint canonicalizes both sides — string literals, numbers, `?`, `$1` and
`:name` all collapse to a single token, harness `RESET ALL;` / `SET …;`
prefixes are stripped, case is folded, punctuation is spaced and whitespace
collapsed — so what is compared is the statement's identifier-and-operator
skeleton. Changing a search term, a range bound, a table suffix, or a parameter
binding does not get a denied statement past the lint; that is deliberate,
because a parameterized family is one statement.

**Enforced mechanically, rule 2: no renamed copies.** See "The column-blind
guard" below.

**Disclosed, not enforced: shape-class overlap.** The corpus deliberately
exercises the operator classes the product is measured on — grouped
aggregation, per-group distinct, top-N, substring and regexp predicates, joins,
point lookups, indexed updates. There is no honest way to avoid this: choosing
a training workload *is* choosing which code paths get optimized, and a corpus
that exercised nothing the product is used for would produce a worse binary and
a less honest number. What we can and do avoid is training on the benchmark's
queries, literally or renamed, or on its data. The distinction to state when
quoting a number: the binary was trained on the *kinds* of work the benchmark
does, over data and statements of its own — never on the benchmark's
statements or tables.

**Enforced by review, rule 3: no statement chosen by a benchmark's numbers.** A
statement is not added because a published query's instruction count or time
lagged after a PGO build, and a statement that reproduces a published query's
distinctive structure under other names goes even when the lint passes it.
Statements removed under this rule: the per-query additions made after a
profile comparison (grouped distinct over very many text groups, composite keys
with an expression member, regexp host and family keys, expression keys over
one dictionary-coded column, lone grouped distinct counts, columnar point
probes by a unique key), a URL-host key over a referrer column, a single text
column ordered by itself under a predicate on itself (two statements), and a
grouped aggregate with a bare `LIMIT` and no `ORDER BY`.

## The column-blind guard

Rule 2 compares a second form of every statement: its **column-blind
skeleton**. Starting from the canonical form, every identifier that is not an
SQL keyword and not a function name (a word followed by `(`) becomes the one
token `I` — table names, column names and aliases alike. A few rewrites that
change no statement's shape are folded as well, so they cannot be used to step
around the rule: `AS`, `ASC`, `INNER` and `OUTER` are dropped; a qualified
name (`t.col`) is one identifier; an alias after a column, a `)`, `END` or a
literal is dropped; `!=` is `<>`; a negative literal, a decimal literal and a
list of literals (`IN (1, 2, 3)`) are one literal; the field of
`extract(<field> FROM …)` is a literal, as `date_trunc`'s already is. A
training statement whose skeleton equals a denied statement's fails the lint
with a `NEAR-COPY` line naming both statements and the shared skeleton.

The lint also reads a statement whole when it spans lines (it runs to a line
ending in `;` or carrying a psql meta-command such as `\parse`). Before this, a
denied statement reflowed onto two lines passed: each line was compared alone.

Which denylists it applies to: the files carrying a
`-- pgo-lint: column-blind` line and the files carrying a `-- pgo-lint:
skeletons` line (the TPC-H, TPC-DS and analytical vectors, see "Skeleton
denylists" below). Which training files: everything except the transactional
family (`oltp-*`).

**The transactional exception.** The OLTP statements are a few tokens long, and
at that length a column-blind skeleton is shared by every application that has
a table: a primary-key point lookup is `SELECT I FROM I WHERE I = @`, which the
published analytical vector also issues as a selective probe, and a whole-table
count is `SELECT count(*) FROM I`, which it also issues. Applied there, the rule
would ban the point lookup from OLTP training, which is no protection for the
analytical numbers (the analytical probe is a full columnar scan with a filter;
the OLTP lookup is a btree descent on a heap) and a real loss for the
transactional ones. So `oltp-*` files are held to rule 1 against every
denylist, and their column-blind matches are printed as `NOTE` lines and
counted in the `PROOF` line, never silently dropped. Today that is 2 skeletons
(the point lookup and the whole-table count), 3 statements in
`corpus/oltp-generic.sql`. The OLTP denylist itself is not marked
column-blind, for the same reason. A skeletons file carries no statement text;
rule 1 reads its `-- pgo-lint: exact-sha256` lines instead (the TPC-H and TPC-DS
files carry one per statement), so a verbatim published statement in an
`oltp-*` file still fails. Against the analytical skeletons file, which carries
no hashes, an `oltp-*` statement can only produce a `NOTE` (a verbatim
analytical statement could not run in the transactional fixture, whose tables
are its own).

Results, from this tree:

```text
$ pgo/lint-training-overlap.sh                                     # the corpus of record
pgo-lint: PROOF disjoint - 148 training statements (146 distinct canonical forms) over 4 file(s);
          0 of 148 denied canonical forms present; 0 of 156 denied column-blind skeletons present
          (117 statements checked column-blind); 3 transactional statement(s) in 2 skeleton(s)
          noted, not enforced                                                           exit 0
$ pgo/lint-training-overlap.sh --self-test                         # seeded copies must fail
pgo-lint: self-test PASS (6 cases)                                                      exit 0
```

What it does not catch: a published query restructured rather than renamed —
extra or fewer predicates, a different aggregate list, `ORDER BY 2` for
`ORDER BY count(*)`. The guard is a floor, not the argument; the argument is
that the corpus is written from the class list below over a fixture that is
not any benchmark's data, and rule 3 above. (A fuzzier mechanical rule was
tried — token edit distance between skeletons — and does not separate the two:
generic statements such as a tenant top-N by a sum sit closer to published ones
than the removed per-query additions did.)

## Skeleton denylists

A denylist file carrying a `-- pgo-lint: skeletons` line holds column-blind
skeletons instead of statements, one per line, exactly as
`pgo/lint-training-overlap.sh --skeletons <file>` prints them: every table,
column and alias is `I`, every literal `@`. Equal statements have equal
skeletons, so such a file denies verbatim copies as well as renamed ones while
carrying no statement text, table or column name. The lint refuses to run (exit
2) if a line is not the skeleton of itself, so a hand-edited line that could
never match cannot weaken the check silently. To regenerate the analytical
file, run `--skeletons` over the published query file, `sort -u`, and keep the
header.

A denylist file may also carry `-- pgo-lint: exact-sha256 <hex>` lines, one per
denied statement, as `pgo/lint-training-overlap.sh --exact-hashes <file>` prints
them: the sha256 of the statement's canonical form (rule 1). The lint hashes
every training statement's canonical form the same way, so an exact copy fails
in every training file, `oltp-*` included, while the denylist holds no
statement text.

### Checking against your own queries

Any query file can be the denylist: put it, one statement per line with a
`-- pgo-lint: column-blind` line, in a directory and run
`PGO_DENYLIST_DIR=<dir> pgo/lint-training-overlap.sh`. Exit 0 means no corpus
statement is one of yours, verbatim or with its columns renamed.

## The fixture

`corpus/analytics-pulse-fixture.sql` builds "pulse", the event log of a
multi-tenant SaaS product, entirely in SQL: `generate_series`, integer hashing
(`hashint8extended` with fixed seeds), `md5` of the row number, and fixed word
arrays. No `random()`, no clock, no downloaded data, so every training host
(the release builder, a local `build-pgo.sh` run) builds the same rows. None of
the tables or columns come from a published benchmark's schema.

| relation | access method | rows (default) | load path |
|---|---|---|---|
| `events` | `pgrcolumnar`, `sorted_by = 'occurred_on, tenant_id, occurred_at'` | 8,000,000 | `COPY (generator) TO` a scratch file, `COPY FROM` it (the parallel text load) |
| `app_users` | `pgrcolumnar`, `sorted_by = 'user_id'` | 1,600,000 | the same, via its own scratch file |
| `tenants` | `pgrcolumnar`, `sorted_by = 'tenant_id'` | 20,000 | `CREATE TABLE … AS` (the CTAS receiver; nullable columns) |
| `event_kinds` | heap, primary key | 400 | `INSERT … SELECT` |
| `app_releases` | heap, text primary key | 205 | `INSERT … SELECT` (35 of the 240 versions events use are missing) |
| `events_heap` | heap; unique btree on `event_id`, btree on `(user_id, occurred_at)` and on `request_id` | 1,600,000 | `INSERT … SELECT` from `events` (its first fifth by id) |

`events` is 36 columns wide and declares `NOT NULL` wherever the data has no
NULLs (`int2`, `int4`, `int8`, `numeric(12,2)`, `float8`, `real`, `boolean`,
`date`, `timestamp`, `timestamptz`, and `text COLLATE "C"`). The data shapes
are chosen for the engine's codecs and kernels:

- **dictionary text, small**: `platform` (7), `http_method` (5), `country_code`
  (40, skewed), `app_version` (240, skewed toward new), `user_agent` (up to ~16,000
  distinct long strings, 45-115 bytes).
- **dictionary text, large** (long entry windows — the D4652 lesson: the
  entry-window fill trains vectorized only if the profile sees long windows):
  `page_path` — 55 % of rows from 1,200 static routes, the rest entity routes
  whose ids recur within ~3 days, ~1 M distinct; `session_key` — ~6 rows per
  value, ~1.3 M distinct, every repeat within ~70 rows.
- **unique text** (not dictionary-coded): `request_id` (`md5`, 8 M distinct),
  `message` (a log line carrying the row number), `app_users.email` (1.6 M).
- **long and sparse text**: `payload` (JSON-shaped, ~95 bytes, 25 % of rows),
  `stack_trace` (0.3-11 KB, 0.6 % of rows — the oversize path).
- **NULLs** in `user_id`, `referrer`, `search_query` (87 %), `payload`,
  `stack_trace`, `consent`, `amount` / `currency` (97 %), `experiment_bucket`.
- **skew**: tenants, users, event kinds, countries and versions are products of
  uniforms (heavy heads, long tails); `status_code` is 88 % `200`.
- **sorted** `occurred_on` / `occurred_at` / `event_id` (time increases with the
  row number), and everything else unsorted.

Scale is one knob: `PGO_FIXTURE_EVENTS` (default 8000000; `app_users` and
`events_heap` are a fifth of it), `PGO_FIXTURE_TENANTS` (default 20000).
`gen-corpus.sh` writes them, and the two scratch-file paths, as `\set` lines at
the top of the emitted `analytics-fixture.sql`. The scratch files are written
and read by the server (`COPY … TO/FROM` a file: superuser, server on the same
host as the corpus directory), ~3.7 GB for `events` at the default scale; the
caller deletes `<corpus>/fixture-*.tsv` after the load. The generators run at
width 0, so the scratch files, and so the loads, are in the same row order on
every run (the parallel `COPY FROM` decides its own part split).

Measured on the instrumented server (Graviton4 c8g.16xlarge, default scale,
2026-10-19): fixture load 142 s (generation dominates), serial vector 52 s,
runtime vector 27 s, transactional family 36 s, 0 errors in all; the whole
`build-pgo.sh` run, both LTO builds included, took about 35 min there. Halve
`PGO_FIXTURE_EVENTS` to roughly halve the data-dependent part.

## Shape classes covered

Analytical corpus (`corpus/analytics-pulse.sql`, ids `J1`…`J88`), written from
this list — no statement started as a published query:

| ids | class |
|---|---|
| J1 | range predicate on the leading sort key (part and granule pruning) + count |
| J2–J5 | null-aware counting (`FILTER`, `count(col)`); scalar aggregate battery over int2 / int4 / int8 / float8 / real / numeric; statistical and boolean aggregates; text `min` / `max` (dictionary and random) |
| J6–J9 | global `COUNT(DISTINCT)`: wide integer under an `IN` list; four dictionary-coded text columns in one pass (the every-lane entries window, two of them large dictionaries); a large dictionary with a predicate on itself; random high-cardinality text and a wide integer |
| J10–J12 | per-group `COUNT(DISTINCT)`: low-cardinality text key, sorted date key, `HAVING` on the distinct count |
| J13–J19 | `GROUP BY` at low (smallint; composite text), medium (tenant) and very high cardinality (int8 user, text session, unique md5 text); large-dictionary text key under a prefix filter |
| J20–J24 | expression keys: `extract`, `date_trunc`, `CASE` buckets, `split_part`, arithmetic bins |
| J25–J31 | `HAVING` over a derived rate; NULL groups; ordered-set aggregates; `date_trunc` on `timestamptz`; `string_agg` / `array_agg` with `DISTINCT … ORDER BY`; `FILTER` pivot; aggregate over a grouped subquery |
| J32–J39 | top-N: integer, timestamp, sorted `DISTINCT` over a NULL-heavy dictionary column, mixed-direction multi-key, deep `OFFSET`, numeric `NULLS LAST`, two text keys, a full sort with no `LIMIT` |
| J40–J48 | string predicates and keys: `LIKE` / `NOT LIKE`, `ILIKE`, anchored regexp, `regexp_replace` template key, `~*`, length statistics, `strpos` / `to_char` / concatenation, JSON-shaped sparse text (regexp substring key), `OR` of disjunctions |
| J49–J58 | joins: fact × columnar dimension, fact × heap dimension, three-way through a 1.6 M-row build side, text-key `LEFT JOIN … IS NULL`, `LEFT JOIN` with NULL probe keys, `EXISTS` semi, `NOT EXISTS` anti, `IN (subquery)`, `NOT IN (subquery)`, self-join on text + integer |
| J59–J62 | correlated scalar subquery in `WHERE` (decorrelated to a grouped join; bounded outer), correlated scalar subquery in the select list, uncorrelated percentile subquery, heap × columnar join |
| J63–J67 | windows: `rank` over grouped partitions, `row_number`, running and moving sums (`ROWS` frame), `lag` / `dense_rank` / `ntile` over a named window, framed `first_value` / `max` |
| J68–J74 | CTE, `MATERIALIZED` CTE read three times, `UNION ALL`, `UNION`, `INTERSECT`, `EXCEPT`, recursive CTE |
| J75–J78 | `ROLLUP`, `GROUPING SETS` with `grouping()`, `CUBE` over `JOIN … USING`, `DISTINCT ON` |
| J79–J81 | wide projections: `SELECT *` under a text range, `SELECT *` of a sorted-key slice with top-N, `e.*` plus dimension columns |
| J82–J87 | heap: unique-index point lookup, `IN` list of lookups, composite-index backward range top-N, text-index lookup with a computed key, sequential scan into a group, index nested loop from a columnar outer |
| J88 | interval arithmetic across `timestamptz` and `timestamp` |

Transactional corpus (`corpus/oltp-generic.sql`, ids `O1`…`O20`): unique-key
lookup, bounded key-range scan, range-fed scalar aggregate, range-fed sort on a
fixed-width string, range-fed sorted distinct, secondary-index range count,
indexed-column update, non-indexed wide-column update, balance update with
in-transaction read-back, small- and very-small-relation contended updates,
append-only insert, delete/re-insert pair, whole-relation count, whole-relation
top-N, constant projection, multi-row `VALUES` insert, bulk `COPY`, and
explicit `BEGIN`/`COMMIT` blocks wrapping the above.

### Engine classes

Every analytical statement trains twice. The serial vector runs it at width 0
(`max_parallel_workers_per_gather = 0`: the serial kernels);
`corpus/analytics-pulse-engines.tsv` picks the runtime vector's width: `w8`
(8 workers: the pool's claims, the wide merge and count passes) or `default`
(the server's own setting). Assignment is **by shape class** — it is not copied
from any published per-query vector: point and index lookups, correlated
subqueries, the recursive CTE, per-row windows, `DISTINCT ON`, ordered-set and
string aggregates and the unbounded sort train at the server default (16 of
88); the scans, aggregates, joins and sorts over the fact table train at
width 8. The mix exists so the profile covers the serial
kernels, the wide pool paths and the server-default width rather than only
whichever one the current planner happens to pick.

## Denylist sources

`tpch-official.sql` and `tpcds-official.sql` are generated from the 22 TPC-H
and 99 TPC-DS query templates as the project's runners issue them, one
statement per line (comments dropped, whitespace collapsed): the header, then
`--skeletons <query file> | sort -u`, then `--exact-hashes <query file>`. The
query text itself is not kept in this tree. `analytics-skeletons.sql` is
generated as described under "Skeleton denylists". Regenerate a file when its query set changes, and keep the
`-- pgo-lint: column-blind` (or `-- pgo-lint: skeletons`) line in the header of
every analytical denylist (the lint refuses to run, exit 2, if no denylist
carries one).

## Running it

```sh
pgo/lint-training-overlap.sh              # lint the checked-in corpus
pgo/gen-corpus.sh /tmp/corpus             # expand, then lint the expansion
pgo/lint-training-overlap.sh <file>...    # lint arbitrary training text
pgo/lint-training-overlap.sh --skeletons <file>...   # print column-blind skeletons
pgo/lint-training-overlap.sh --exact-hashes <file>...   # print exact-sha256 denylist lines
pgo/lint-training-overlap.sh --self-test               # prove the lint catches seeded copies
```

Exit 0 = disjoint (prints a `PROOF` line naming the corpus sizes, the denied
canonical forms and column-blind skeletons, and how many statements were
checked column-blind). Exit 1 = overlap or near-copy, with the offending
statement, the denylist file that caught it, and the canonical form or
skeleton both share. Exit 2 = the lint could not run — never treat that as a
pass.

`gen-corpus.sh` emits `analytics-fixture.sql`, `analytics-serial.sql` and
`analytics-runtime.sql` next to the transactional legs. A training run loads
the fixture once into an empty database and runs both vectors there:

```sh
psql -d postgres -c 'CREATE DATABASE pgotrain'
psql -d pgotrain -v ON_ERROR_STOP=1 -f /tmp/corpus/analytics-fixture.sql && rm -f /tmp/corpus/fixture-*.tsv
psql -d pgotrain -f /tmp/corpus/analytics-serial.sql
psql -d pgotrain -f /tmp/corpus/analytics-runtime.sql
```

`scripts/release/pgo-train.sh` does exactly this (database `pgotrain`, in a
fresh `initdb` cluster), counts the errors per vector, and stops if the fixture
did not load or every statement of a vector errored.

`gen-corpus.sh` is deterministic: given the same output directory it produces
byte-identical files (fixed LCG seeds and hash seeds, no clock, no randomness).
The path-dependent lines are the `COPY` source in `oltp-main.sql`, which names
the emitted `copy.dat`, and the two scratch-file `\set` lines at the top of
`analytics-fixture.sql`; the corpus sha256 does not depend on them.

### Reproducing a PGO binary

A PGO artifact is reproducible from (source sha, corpus sha256), and the corpus
is in the tree at that sha:

1. check out the sha the binary was built at;
2. `benchmarks/build-pgo.sh` on a Linux host with the pinned toolchain and the
   PostgreSQL 18 client tools: it lints, builds instrumented, trains with
   `scripts/release/pgo-train.sh` (which re-expands and re-lints the corpus, so
   the non-overlap check is part of the reproduction, not a claim about it),
   merges, rebuilds with the merged profile and writes `build-manifest.txt`;
3. compare the manifest's `corpus_sha256` with the `PGO-CORPUS` line of the
   release's training log.

## Adding to the corpus

Add the statement to `corpus/`, give it an id and a one-line shape-class
comment, add its engine class if it is analytical, and run the lint. Start from
the class you want trained, not from a query you have seen measured: if the
lint rejects it — as a published statement or as a renamed one — rewrite it
from the class rather than renaming its way past the rule or adding an
exemption. Never add a statement because a published query's number lagged
after a PGO build (rule 3): a class that is under-trained shows up on the
corpus itself and on the product's own workloads, and that is the evidence a
new statement needs. Exemptions in `denylist/EXEMPTIONS.txt` are for statements that are
genuinely content-free; they name a training statement (its exact canonical
form) and apply to either rule, and every one that is applied is printed, so
none is silent.

Any change here changes the profile, and therefore the binary. Treat it like a
codegen change: it needs a measured before/after, not just a green lint.

## When a corpus statement crashes the engine

It happens — a corpus that is not the benchmark reaches code the benchmark
does not. `corpus/QUARANTINE.tsv` holds shape ids that are kept out of the
emitted training vectors, one per line with the reason and the defect to close.
A quarantined statement stays in `corpus/`, stays linted, and stays documented;
`gen-corpus.sh` prints a loud `QUARANTINED <id> — <reason>` line for every
exclusion, so it appears in the build log and cannot rot into silence.

Quarantine is for statements the engine cannot currently survive. It is not for
statements that are slow, awkward, or inconvenient, and a line with no defect
named is a bug in the process.

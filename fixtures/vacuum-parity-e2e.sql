-- lane/vacuum-parity: regression (eager-scan success cap on the
-- default morsel arm). Expected captured from C 18.6.
\set VERBOSITY verbose
-- A cached all-visible table wider than 2 * EAGER_SCAN_REGION_SIZE pages:
-- the success cap is 0.2 * relallvisible = 2000 freezes; C's read stream
-- hands out block N+1 before block N is accounted, so 2001 pages are eagerly
-- scanned and frozen, plus the last page (never skippable); eager scanning
-- is then disabled and the remaining all-visible pages are skipped, so
-- relfrozenxid cannot advance. The plain VACUUM runs pgrust's default
-- (morsel) scan arm with max_parallel_maintenance_workers helpers.
CREATE TABLE vacpar_es (a int, b text);
INSERT INTO vacpar_es SELECT g, repeat('x', 500) FROM generate_series(1, 150000) g;
VACUUM vacpar_es;
SELECT txid_current() > 0;
CREATE TEMP TABLE vacpar_before AS SELECT relfrozenxid FROM pg_class WHERE relname = 'vacpar_es';
SET vacuum_freeze_min_age = 0;
VACUUM vacpar_es;
RESET vacuum_freeze_min_age;
SELECT relallfrozen, relallvisible, relpages FROM pg_class WHERE relname = 'vacpar_es';
SELECT c.relfrozenxid = b.relfrozenxid AS relfrozenxid_unchanged FROM pg_class c, vacpar_before b WHERE c.relname = 'vacpar_es';
-- The same table under PARALLEL 1 (one helper) and PARALLEL 0 (serial arm):
-- the count is a property of the caps, not of the claim schedule.
UPDATE vacpar_es SET a = a WHERE a % 7 = 0;
VACUUM vacpar_es;
SET vacuum_freeze_min_age = 0;
VACUUM (PARALLEL 1) vacpar_es;
RESET vacuum_freeze_min_age;
SELECT relallfrozen, relallvisible, relpages FROM pg_class WHERE relname = 'vacpar_es';
DROP TABLE vacpar_es;

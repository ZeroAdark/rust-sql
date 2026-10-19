-- bugs/batch-220-backend-access-gin: C-vs-pgrust parity for the fixes in
-- access/gin. Expected file captured from C 18.6
-- (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- GinFormTuple forms multicolumn entry tuples with initGinState's per-column
-- tupdesc (TupleDescInitEntry: attstorage = the key type's typstorage, no
-- compression method), so a btree_gin text key wider than TOAST_INDEX_TARGET
-- is pglz-compressed in-line even when the index column itself was created
-- with STORAGE EXTERNAL or PLAIN; keys of ~3 KB fit and are found again.
CREATE EXTENSION btree_gin;

CREATE TABLE b220_ext (a int, b text);
ALTER TABLE b220_ext ALTER COLUMN b SET STORAGE EXTERNAL;
CREATE INDEX b220_ext_gin ON b220_ext USING gin (a, b);
INSERT INTO b220_ext VALUES (1, repeat('x', 3000));
INSERT INTO b220_ext VALUES (2, repeat('y', 1000));
INSERT INTO b220_ext VALUES (3, 'short');
SET enable_seqscan = off;
SELECT a, length(b) FROM b220_ext WHERE b = repeat('x', 3000);
SELECT a, length(b) FROM b220_ext WHERE b = repeat('y', 1000);
SELECT a, b FROM b220_ext WHERE b = 'short';
SELECT count(*) FROM b220_ext WHERE a = 1;
RESET enable_seqscan;

CREATE TABLE b220_plain (a int, b text STORAGE PLAIN);
CREATE INDEX b220_plain_gin ON b220_plain USING gin (a, b);
INSERT INTO b220_plain VALUES (1, repeat('z', 2600));
SET enable_seqscan = off;
SELECT a, length(b) FROM b220_plain WHERE b = repeat('z', 2600);
RESET enable_seqscan;

-- STORAGE changed after the index exists (SetIndexStorageProperties pushes
-- the new storage into the index column): still compressed in the entry tuple.
CREATE TABLE b220_alter (a int, b text);
CREATE INDEX b220_alter_gin ON b220_alter USING gin (a, b);
ALTER TABLE b220_alter ALTER COLUMN b SET STORAGE EXTERNAL;
INSERT INTO b220_alter VALUES (1, repeat('w', 3000));
SET enable_seqscan = off;
SELECT a, length(b) FROM b220_alter WHERE b = repeat('w', 3000);
RESET enable_seqscan;

-- An incompressible key over the limit still errors identically.
CREATE TABLE b220_big (a int, b text);
CREATE INDEX b220_big_gin ON b220_big USING gin (a, b);
INSERT INTO b220_big SELECT 1, string_agg(md5(i::text), '') FROM generate_series(1, 100) i;

DROP TABLE b220_ext, b220_plain, b220_alter, b220_big;
DROP EXTENSION btree_gin;

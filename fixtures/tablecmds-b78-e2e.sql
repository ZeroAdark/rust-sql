-- bugs/batch-78-backend-commands-tablecmds: single-session divergences in
-- tablecmds, expected captured from C PostgreSQL 18.6.
-- regression: inherited cooked defaults are stored before inherited
-- CHECKs (tablecmds.c:1073 list_concat(cookedDefaults, old_constraints)).
CREATE TABLE b78_p1(a integer DEFAULT 1 CHECK (a > 0));
CREATE TABLE b78_c1() INHERITS (b78_p1);
SELECT d.oid < k.oid AS default_created_first FROM pg_attrdef d JOIN pg_constraint k ON k.conrelid = d.adrelid WHERE d.adrelid = 'b78_c1'::regclass AND k.contype = 'c';
CREATE TABLE b78_pp1(k int, a integer DEFAULT 1 CHECK (a > 0)) PARTITION BY RANGE (k);
CREATE TABLE b78_cp1 PARTITION OF b78_pp1 FOR VALUES FROM (0) TO (10);
SELECT d.oid < k.oid AS default_created_first FROM pg_attrdef d JOIN pg_constraint k ON k.conrelid = d.adrelid WHERE d.adrelid = 'b78_cp1'::regclass AND k.contype = 'c';
-- regression: a partition's columns carry attndims 0
-- (MergeAttributes makeColumnDef, tablecmds.c:2790).
CREATE TABLE b78_p2(k integer, a integer[][]) PARTITION BY RANGE(k);
CREATE TABLE b78_c2 PARTITION OF b78_p2 FOR VALUES FROM (0) TO (10);
SELECT attname, attndims FROM pg_attribute WHERE attrelid = 'b78_c2'::regclass AND attnum > 0 ORDER BY attnum;
-- regression: the tablespace is resolved before the
-- partition-parent ownership check (tablecmds.c:889 vs 2745).
CREATE TABLE public.b78_p3(k integer) PARTITION BY RANGE(k);
CREATE ROLE b78_u;
CREATE SCHEMA b78_s AUTHORIZATION b78_u;
SET ROLE b78_u;
CREATE TABLE b78_s.c PARTITION OF public.b78_p3 DEFAULT TABLESPACE b78_nonexistent_tablespace;
CREATE TABLE b78_s.c PARTITION OF public.b78_p3 DEFAULT;
RESET ROLE;
DROP SCHEMA b78_s;
DROP ROLE b78_u;
-- regression: unsupported PERIOD actions are rejected before
-- the column-count check (tablecmds.c:10288-10320).
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE TABLE b78_pk4(id int, r int4range, PRIMARY KEY (id, r WITHOUT OVERLAPS));
CREATE TABLE b78_fk4(a int, b int, r int4range);
ALTER TABLE b78_fk4 ADD FOREIGN KEY(a, b, PERIOD r) REFERENCES b78_pk4(id, PERIOD r) ON UPDATE CASCADE;
ALTER TABLE b78_fk4 ADD FOREIGN KEY(a, b, PERIOD r) REFERENCES b78_pk4(id, PERIOD r) ON DELETE SET NULL;
ALTER TABLE b78_fk4 ADD FOREIGN KEY(a, b, PERIOD r) REFERENCES b78_pk4(id, PERIOD r);
ALTER TABLE b78_fk4 ADD FOREIGN KEY(a, PERIOD r) REFERENCES b78_pk4(id, PERIOD r) ON UPDATE CASCADE;
-- regression: VALIDATE CONSTRAINT of an inherited NOT NULL
-- runs set_attnotnull's CheckAlterTableIsSafe on every child (tablecmds.c:7856).
CREATE TABLE b78_p5 (a int);
CREATE TABLE b78_c5 () INHERITS (b78_p5);
ALTER TABLE b78_p5 ADD CONSTRAINT nn NOT NULL a NOT VALID;
ALTER TABLE b78_p5 ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
INSERT INTO b78_c5 VALUES (1);
BEGIN;
DECLARE b78_cur CURSOR FOR SELECT * FROM ONLY b78_c5;
ALTER TABLE b78_p5 VALIDATE CONSTRAINT nn;
ROLLBACK;
BEGIN;
DECLARE b78_cur CURSOR FOR SELECT * FROM ONLY b78_c5;
ALTER TABLE b78_p5 VALIDATE CONSTRAINT ck;
ROLLBACK;
BEGIN;
DECLARE b78_cur CURSOR FOR SELECT * FROM ONLY b78_p5;
ALTER TABLE b78_p5 VALIDATE CONSTRAINT nn;
ROLLBACK;
ALTER TABLE b78_p5 VALIDATE CONSTRAINT nn;
ALTER TABLE b78_p5 VALIDATE CONSTRAINT ck;
SELECT conname, convalidated FROM pg_constraint WHERE conrelid = 'b78_c5'::regclass ORDER BY conname;
-- regression: RememberAllDependentForRebuilding keeps its
-- pg_depend RowExclusiveLock to end of transaction (tablecmds.c:15282).
CREATE TABLE b78_p6(a varchar);
BEGIN;
ALTER TABLE b78_p6 ALTER COLUMN a TYPE text;
SELECT mode FROM pg_locks WHERE pid = pg_backend_pid() AND relation = 'pg_catalog.pg_depend'::regclass;
ROLLBACK;
-- regression: ATPrepChangePersistence errors carry
-- errtable / errtableconstraint fields (tablecmds.c:18887, 18959, 18969).
CREATE UNLOGGED TABLE b78_p8 (id int PRIMARY KEY);
CREATE UNLOGGED TABLE b78_c8 (id int CONSTRAINT b78_fk8 REFERENCES b78_p8);
ALTER TABLE b78_c8 SET LOGGED;
CREATE TABLE b78_l8 (id int PRIMARY KEY);
CREATE TABLE b78_lc8 (id int CONSTRAINT b78_fk8b REFERENCES b78_l8);
CREATE TEMP TABLE b78_t8(a int);
CREATE FUNCTION b78_diag(cmd text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE s text; t text; c text; e text;
BEGIN
  EXECUTE cmd;
  RETURN 'no error';
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS s = SCHEMA_NAME, t = TABLE_NAME, c = CONSTRAINT_NAME, e = RETURNED_SQLSTATE;
  RETURN format('%s schema=%s table=%s constraint=%s', e, regexp_replace(s, '^pg_temp_\d+$', 'pg_temp_N'), t, c);
END $$;
SELECT b78_diag('ALTER TABLE b78_c8 SET LOGGED');
SELECT b78_diag('ALTER TABLE b78_l8 SET UNLOGGED');
SELECT b78_diag('ALTER TABLE b78_t8 SET LOGGED');
SELECT b78_diag('ALTER TABLE b78_t8 SET UNLOGGED');
DROP FUNCTION b78_diag(text);
-- regression: ALTER COLUMN TYPE ... COLLATE lookup failures carry
-- the COLLATE clause position (parse_type.c:520 LookupCollation).
CREATE TABLE b78_coll(a text);
ALTER TABLE b78_coll ALTER COLUMN a TYPE text COLLATE b78_nonexistent_collation;
ALTER TABLE b78_coll ALTER COLUMN a TYPE text COLLATE b78_nonexistent_schema.b78_nonexistent_collation;
ALTER TABLE b78_coll ALTER COLUMN a TYPE int COLLATE "C";
DROP TABLE b78_coll, b78_p8, b78_c8, b78_l8, b78_lc8, b78_p6, b78_p5, b78_c5, b78_pk4, b78_fk4, b78_p3, b78_p2, b78_pp1, b78_p1, b78_c1;

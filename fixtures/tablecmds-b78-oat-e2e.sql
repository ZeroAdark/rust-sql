-- bugs/batch-78-backend-commands-tablecmds: OAT_POST_ALTER hook sites that
-- ALTER TABLE forms skipped, witnessed through the test_oat_hooks recorder.
-- The C oracle install carries no test modules, so the expected file is
-- captured from pgrust and the NOTICE shape is test_oat_hooks.c's.
LOAD 'test_oat_hooks';
CREATE TABLE b78_oat_p(a integer);
CREATE TABLE b78_oat_c(a integer);
CREATE TYPE b78_oat_ct AS (a integer);
CREATE TABLE b78_oat_t(a integer);
CREATE TABLE b78_oat_pt(a integer) PARTITION BY RANGE(a);
CREATE TABLE b78_oat_v (a int);
ALTER TABLE b78_oat_v ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
ALTER TABLE b78_oat_v ADD CONSTRAINT nn NOT NULL a NOT VALID;
CREATE FOREIGN DATA WRAPPER b78_oat_fdw;
CREATE SERVER b78_oat_srv FOREIGN DATA WRAPPER b78_oat_fdw;
CREATE FOREIGN TABLE b78_oat_ft(a integer) SERVER b78_oat_srv;
CREATE TABLE b78_oat_rw(a text);
INSERT INTO b78_oat_rw VALUES ('x');
SET test_oat_hooks.audit = on;
-- regression / p7#3: StoreCatalogInheritance1 (tablecmds.c:3602)
ALTER TABLE b78_oat_c INHERIT b78_oat_p;
-- regression: ATExecAddOf / ATExecDropOf (tablecmds.c:18388, 18433)
ALTER TABLE b78_oat_t OF b78_oat_ct;
ALTER TABLE b78_oat_t NOT OF;
-- regression: ATExecSetTableSpaceNoStorage no-op arm (tablecmds.c:16999)
ALTER TABLE b78_oat_pt SET TABLESPACE pg_default;
-- regression: Queue{Check,NN}ConstraintValidation (tablecmds.c:13228, 13331)
ALTER TABLE b78_oat_v VALIDATE CONSTRAINT ck;
ALTER TABLE b78_oat_v VALIDATE CONSTRAINT nn;
-- regression: ATExecAlterColumnGenericOptions (tablecmds.c:16086)
ALTER FOREIGN TABLE b78_oat_ft ALTER COLUMN a OPTIONS (ADD remote_name 'a');
-- regression / regression: finish_heap_swap's TOAST
-- renames are is_internal (cluster.c:1603, tablecmds.c:4337)
ALTER TABLE b78_oat_rw SET UNLOGGED;
SET test_oat_hooks.audit = off;
DROP TABLE b78_oat_rw, b78_oat_c, b78_oat_p, b78_oat_t, b78_oat_pt, b78_oat_v;
DROP FOREIGN TABLE b78_oat_ft;
DROP SERVER b78_oat_srv;
DROP FOREIGN DATA WRAPPER b78_oat_fdw;
DROP TYPE b78_oat_ct;

-- bugs/batch-230-mixed-lowtail (2026-09-13 bug inventory). Expected captured
-- from C PostgreSQL 18.6 via scripts/regress-diff.sh --capture with
-- B230_EXT_DIR=fixtures/b230ext exported (scripts/bugs-batch230-e2e.sh).
\set VERBOSITY verbose
\set SHOW_CONTEXT always
-- regression: script_error_callback decorates a WARNING
-- raised inside an extension script (extension.c:1062-1065).
\getenv b230_ext B230_EXT_DIR
\set b230_ecp '$system:' :b230_ext
SET extension_control_path = :'b230_ecp';
CREATE EXTENSION b230ext;
DROP EXTENSION b230ext;
RESET extension_control_path;
-- regression: the missing-default-opclass error formats the
-- type under the DDL issuer's search_path (indexcmds.c:2295).
CREATE TYPE public.b230_ct AS (a int, b int);
CREATE TABLE b230_ot(c public.b230_ct);
CREATE INDEX ON b230_ot USING gist (c);
CREATE INDEX ON b230_ot USING hash (c);
-- regression: the conflict error names the relation
-- opened most recently (publicationcmds.c:1763/1775).
CREATE TABLE b230_par(a int, b int);
CREATE TABLE b230_ch0(a int, b int) INHERITS (b230_par);
CREATE TABLE b230_ch1(a int, b int) INHERITS (b230_par);
CREATE TABLE b230_ch2(a int, b int) INHERITS (b230_par);
CREATE PUBLICATION b230_pub FOR TABLE b230_ch1 WHERE (a > 0), b230_par WHERE (b > 0);
CREATE PUBLICATION b230_pub FOR TABLE b230_ch2 (a), b230_par (b);
CREATE PUBLICATION b230_pub FOR TABLE b230_ch0 WHERE (a > 0), b230_par;
CREATE PUBLICATION b230_pub FOR TABLE b230_ch2 WHERE (a > 0), b230_par;
-- regression: TypeNameToString keeps the array decoration.
CREATE DOMAIN b230_d AS record[];
-- regression: aclcheck_error_type names the element type.
CREATE ROLE b230_u;
CREATE TYPE public.b230_et AS ENUM ('x');
SET ROLE b230_u;
ALTER TYPE public._b230_et RENAME TO b230_zz;
ALTER TYPE public.b230_et RENAME TO b230_zz;
ALTER TYPE public._b230_et OWNER TO b230_u;
ALTER TYPE public._b230_et SET SCHEMA pg_catalog;
ALTER TYPE public._b230_et ADD VALUE 'y';
ALTER TYPE public._b230_et SET (storage = plain);
ALTER DOMAIN public._b230_et SET NOT NULL;
RESET ROLE;
DROP TYPE public.b230_et;
DROP ROLE b230_u;
-- regression: gin_index_check DEBUG1/DEBUG3 walk lines.
CREATE EXTENSION amcheck;
CREATE TABLE b230_g(a int[]);
INSERT INTO b230_g SELECT ARRAY[g, g+1, g % 7] FROM generate_series(1,300) g;
CREATE INDEX b230_gi ON b230_g USING gin(a);
-- (first call loads the module: its find_in_path DEBUG lines are install-specific)
SELECT gin_index_check('b230_gi');
SET client_min_messages = debug3;
SELECT gin_index_check('b230_gi');
RESET client_min_messages;
CREATE TABLE b230_g2(a int[]);
INSERT INTO b230_g2 SELECT ARRAY[1, g % 3] FROM generate_series(1,40000) g;
CREATE INDEX b230_g2i ON b230_g2 USING gin(a);
SET client_min_messages = debug1;
SELECT gin_index_check('b230_g2i');
SET client_min_messages = debug3;
SELECT gin_index_check('b230_g2i');
RESET client_min_messages;

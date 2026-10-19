-- bugs/batch-176-mixed-xhigh: expected captured from C 18.6
-- (scripts/mixed-b176-e2e.sh).
\set VERBOSITY verbose
-- regression: a NaN hour count is out of range (variable.c:332),
-- not a zero UTC offset.
SET timezone = 'nan';
SET timezone = 'NaN';
SET timezone = '-nan';
SET timezone = 'inf';
SET timezone = '-inf';
SET timezone = '1e400';
SET timezone = '3';
SHOW timezone;
RESET timezone;
-- regression: statement-level UPDATE/DELETE events run the RI
-- queue-time check too (trigger.c:6470): an ordinary trigger naming an RI
-- builtin raises 42P17 at queue time, not 39P01 at execution.
CREATE TABLE b176_t (a int);
INSERT INTO b176_t VALUES (1);
CREATE TRIGGER b176_upd AFTER UPDATE ON b176_t FOR EACH STATEMENT EXECUTE FUNCTION "RI_FKey_noaction_upd"();
UPDATE b176_t SET a = 2;
DROP TRIGGER b176_upd ON b176_t;
CREATE TRIGGER b176_del AFTER DELETE ON b176_t FOR EACH STATEMENT EXECUTE FUNCTION "RI_FKey_check_ins"();
DELETE FROM b176_t;
DROP TRIGGER b176_del ON b176_t;
CREATE TRIGGER b176_ins AFTER INSERT ON b176_t FOR EACH STATEMENT EXECUTE FUNCTION "RI_FKey_check_ins"();
INSERT INTO b176_t VALUES (3);
DROP TRIGGER b176_ins ON b176_t;
CREATE TRIGGER b176_updfk AFTER UPDATE ON b176_t FOR EACH STATEMENT EXECUTE FUNCTION "RI_FKey_check_upd"();
UPDATE b176_t SET a = 4;
DROP TRIGGER b176_updfk ON b176_t;
SELECT * FROM b176_t;
CREATE TABLE b176_p (a int) PARTITION BY LIST (a);
CREATE TABLE b176_p1 PARTITION OF b176_p FOR VALUES IN (1);
INSERT INTO b176_p VALUES (1);
CREATE TRIGGER b176_pfk AFTER UPDATE ON b176_p FOR EACH STATEMENT EXECUTE FUNCTION "RI_FKey_check_upd"();
UPDATE b176_p SET a = 1;
CREATE TRIGGER b176_ppk AFTER UPDATE ON b176_p FOR EACH STATEMENT EXECUTE FUNCTION "RI_FKey_noaction_upd"();
UPDATE b176_p SET a = 1;
DROP TABLE b176_p;
-- regression: validateDomainCheckConstraint closes each scanned
-- relation handle (typecmds.c:3304), so a CHECK function may truncate an
-- already-scanned table; the same for the NOT NULL validator.
CREATE DOMAIN b176_d AS int;
CREATE TABLE b176_d1 (x b176_d);
CREATE TABLE b176_d2 (x b176_d);
INSERT INTO b176_d1 VALUES (1);
INSERT INTO b176_d2 VALUES (2);
CREATE FUNCTION b176_f(int) RETURNS bool LANGUAGE plpgsql AS $$ BEGIN IF $1 = 2 THEN EXECUTE 'TRUNCATE b176_d1'; END IF; RETURN true; END $$;
ALTER DOMAIN b176_d ADD CONSTRAINT b176_c1 CHECK (b176_f(VALUE));
SELECT count(*) FROM b176_d1;
SELECT count(*) FROM b176_d2;
ALTER DOMAIN b176_d SET NOT NULL;
INSERT INTO b176_d1 VALUES (5);
INSERT INTO b176_d1 VALUES (NULL);
DROP TABLE b176_d1, b176_d2;
DROP DOMAIN b176_d;
DROP FUNCTION b176_f(int);
DROP TABLE b176_t;

-- regression: a pg_proc row with a corrupt prokind is refused with C's
-- "unrecognized prokind: x" and the call's cursor position.
\set VERBOSITY verbose
SET allow_system_table_mods = on;
CREATE FUNCTION cov1768f(int) RETURNS int LANGUAGE sql AS 'select $1';
UPDATE pg_proc SET prokind = 'x' WHERE proname = 'cov1768f';
SELECT cov1768f(1);
SELECT 1 + cov1768f(2);
UPDATE pg_proc SET prokind = 'f' WHERE proname = 'cov1768f';
SELECT cov1768f(3);
CREATE PROCEDURE cvp(IN a int, INOUT b int) LANGUAGE plpgsql AS $$ BEGIN b := a; END $$;
UPDATE pg_proc SET proargmodes = '{i,t}' WHERE proname = 'cvp';
CALL cvp(1, NULL);

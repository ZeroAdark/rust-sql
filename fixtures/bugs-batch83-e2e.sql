-- Differential fixture for the 2026-09-13 audit bug batch 83
-- (backend/libpq: SCRAM secret iteration parsing, hba atoi narrowing, raw
-- authentication-file token bytes in the views, auth-file read failures).
-- Run via scripts/bugs-batch83-e2e.sh against the frozen expected captured
-- from C PostgreSQL 18.6.
\pset pager off

-- regression: strtol accepts LONG_MIN (narrowed to 0 iterations),
-- so the supplied verifier is stored verbatim; one past it is ERANGE and the
-- text is treated as a plaintext password
CREATE ROLE b83_edge LOGIN PASSWORD 'SCRAM-SHA-256$-9223372036854775808:AA==$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=';
SELECT rolpassword FROM pg_authid WHERE rolname = 'b83_edge';
CREATE ROLE b83_over LOGIN PASSWORD 'SCRAM-SHA-256$-9223372036854775809:AA==$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=';
SELECT rolpassword LIKE 'SCRAM-SHA-256$4096:%' AS rehashed FROM pg_authid WHERE rolname = 'b83_over';
DROP ROLE b83_edge, b83_over;

-- The authentication-file views read the files on disk: rewrite them (raw
-- bytes via lo_export, the directory swap via COPY TO PROGRAM) and restore.
SELECT current_setting('data_directory') AS dd \gset
SELECT format('cd %s && cp -p pg_hba.conf pg_hba.conf.b83 && cp -p pg_ident.conf pg_ident.conf.b83', :'dd') AS cmd_backup \gset
COPY (SELECT 1) TO PROGRAM :'cmd_backup';

-- regression: atoi = (int) strtol, so 4294967296 is port 0 (rejected),
-- 4294967297 is port 1 and -4294967295 is port 1
SELECT lo_from_bytea(0, E'local all all trust\nhost all all 127.0.0.1/32 trust\nhost all all ::1/128 trust\nhost all all 127.0.0.3/32 radius radiusservers=localhost radiussecrets=x radiusports=4294967296\nhost all all 127.0.0.4/32 radius radiusservers=localhost radiussecrets=x radiusports=4294967297\nhost all all 127.0.0.5/32 radius radiusservers=localhost radiussecrets=x radiusports=-4294967295\n'::bytea) AS hba_lo \gset
SELECT lo_export(:hba_lo, :'dd' || '/pg_hba.conf');
SELECT rule_number, line_number, type, database, user_name, address, netmask, auth_method, options, error FROM pg_hba_file_rules ORDER BY line_number;

-- regression: token bytes that are not UTF-8 reach the view unchanged
SELECT lo_from_bytea(0, 'b83map user'::bytea || '\xe9'::bytea || E' role\nb83map sys pguser\n'::bytea) AS ident_lo \gset
SELECT lo_export(:ident_lo, :'dd' || '/pg_ident.conf');
SELECT map_number, line_number, map_name, sys_name, octet_length(sys_name), pg_username, error FROM pg_ident_file_mappings ORDER BY line_number;

-- regression: a file that opens but cannot be read (a directory) is
-- a DEBUG3 "could not read file" for the views: no rows, no error
SELECT format('cd %s && mv pg_hba.conf pg_hba.conf.dir && mkdir pg_hba.conf && mv pg_ident.conf pg_ident.conf.dir && mkdir pg_ident.conf', :'dd') AS cmd_dirs \gset
COPY (SELECT 1) TO PROGRAM :'cmd_dirs';
SELECT count(*) FROM pg_hba_file_rules;
SELECT count(*) FROM pg_ident_file_mappings;

SELECT format('cd %s && rmdir pg_hba.conf pg_ident.conf && mv pg_hba.conf.b83 pg_hba.conf && mv pg_ident.conf.b83 pg_ident.conf && rm pg_hba.conf.dir pg_ident.conf.dir', :'dd') AS cmd_restore \gset
COPY (SELECT 1) TO PROGRAM :'cmd_restore';
SELECT lo_unlink(:hba_lo), lo_unlink(:ident_lo);
SELECT count(*) > 0 AS restored FROM pg_hba_file_rules WHERE error IS NULL;

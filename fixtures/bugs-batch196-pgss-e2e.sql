-- Differential fixture for the 2026-09-13 audit bug batch 196, row
-- regression: inline_function analyzes the candidate body with bare
-- transformTopLevelStmt (clauses.c:4719), so pg_stat_statements never sees an
-- entry for an inline-only body. Run via scripts/bugs-batch196-pgss-e2e.sh
-- (shared_preload_libraries=pg_stat_statements, track=all) against the
-- frozen expected captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off
create extension pg_stat_statements;
select pg_stat_statements_reset() is not null as reset;
set check_function_bodies = off;
create function f1_196(int) returns int language sql stable as 'select $1 + 1';
create function f2_196(int) returns int language sql as 'select 1; SELECT $1 + 1';
select f1_196(x) from (values (1)) v(x);
select f2_196(1);
-- the nested entry keeps f2's text: f1's inline pass registered none
select toplevel, calls, query from pg_stat_statements where query ilike '%$1 + $2%' order by query;

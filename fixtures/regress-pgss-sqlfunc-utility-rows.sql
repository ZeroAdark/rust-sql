-- regression audit XARP: a utility statement run from a LANGUAGE sql function body
-- goes through ProcessUtility with a NULL QueryCompletion (functions.c
-- postquel_getnext), so its nested pg_stat_statements entry records rows 0.
-- Run via scripts/reg-aarp-pgss-sqlfunc-utility-rows-e2e.sh against the
-- expected captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off
create extension pg_stat_statements;
create table src8 as select g as a from generate_series(1, 5) g;
create materialized view mv8 as select * from src8;
create function f_ctas() returns void language sql as $$ create table made as select * from src8 $$;
create function f_into() returns void language sql as $$ select * into made_into from src8 $$;
create function f_refresh() returns void language sql as $$ refresh materialized view mv8 $$;
create function f_copy() returns void language sql as $$ copy src8 to '/dev/null' $$;
select pg_stat_statements_reset() is not null as reset;
select f_ctas();
select f_into();
select f_refresh();
select f_copy();
select toplevel, calls, rows, query from pg_stat_statements where not toplevel order by query collate "C", rows;
do $$ begin execute 'refresh materialized view mv8'; end $$;
select toplevel, calls, rows, query from pg_stat_statements where not toplevel and query like 'refresh%' order by query collate "C";

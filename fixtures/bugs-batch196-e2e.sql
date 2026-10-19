-- Differential fixture for the 2026-09-13 audit bug batch 196
-- (backend/executor/sql_functions: SQL-function error-context frames for
-- warnings and GET DIAGNOSTICS, surplus call arguments, utility statement
-- subcontext, function-name context identifier). Run via
-- scripts/bugs-batch196-e2e.sh against the frozen expected captured from C
-- PostgreSQL 18.6.
\set VERBOSITY verbose
\set SHOW_CONTEXT always
\pset pager off

-- regression: ri_KeysEqual runs the FK-side implicit cast through
-- FunctionCall3 (ri_triggers.c:3079); a one-argument SQL cast function binds
-- the surplus arguments like postquel_sub_params (functions.c:1478)
create type e196 as enum ('a', 'b');
create function e196_to_int(e196) returns int language sql immutable
  as $$ select case when $1::text = 'a' then 1 else 2 end $$;
create cast (e196 as int) with function e196_to_int(e196) as implicit;
create table pk196 (t int primary key);
create table fk196 (id int, v e196 references pk196 (t), x int);
insert into pk196 values (1), (2);
insert into fk196 values (1, 'a', 0);
update fk196 set x = 1 where id = 1;
update fk196 set v = 'a' where id = 1;
update fk196 set v = 'b' where id = 1;
select * from fk196;

-- regression / regression: warnings raised while the body
-- is parsed under CREATE FUNCTION (pg_proc.c:1005) and while it is inlined
-- (clauses.c:4984) are transposed like errors
set standard_conforming_strings = off;
set escape_string_warning = on;
create function w196() returns text language sql as $$ select 'a\\b' $$;
select w196();
select w196() from (values (1)) v(x);
reset standard_conforming_strings;
reset escape_string_warning;
create function w196b() returns void language sql as $$ create global temp table gt196 (a int) $$;
select w196b();
create function w196c() returns setof int language sql as $$ select 1 from (select 1 from gt196 where 'x\\y' = 'x') s $$;
set standard_conforming_strings = off;
select * from w196c();
reset standard_conforming_strings;

-- regression: sql_exec_error_callback (functions.c:1949) is on
-- error_context_stack while the function runs, so nested PL/pgSQL reports and
-- GET [STACKED] DIAGNOSTICS see the SQL-function frame
create function inner196() returns text language plpgsql as $$
declare ctx text;
begin
  begin
    raise exception 'boom';
  exception when others then
    get stacked diagnostics ctx = PG_EXCEPTION_CONTEXT;
    return ctx;
  end;
end $$;
create function outer196() returns text language sql as $$ select 1; select inner196() $$;
select outer196();
create function inner196b() returns text language plpgsql as $$
declare ctx text;
begin
  get diagnostics ctx = PG_CONTEXT;
  return ctx;
end $$;
create function outer196b() returns text language sql as $$ select 1; select inner196b() $$;
select outer196b();
create function inner196c() returns int language plpgsql as $$ begin raise notice 'hi'; return 1; end $$;
create function outer196c() returns int language sql as $$ select 1; select inner196c() $$;
select outer196c();
create function outer196d() returns text language sql as $$ select 1; select inner196() || ' / ' || outer196() $$;
select outer196d();
create function outer196e() returns int language sql as $$ select 1; select 1 / 0 $$;
select outer196e();
-- the frames outside the catching function reach PG_EXCEPTION_CONTEXT; a
-- re-raised error keeps its one copy of them
create function mid196() returns text language plpgsql as $$ begin return inner196(); end $$;
create function mid196b() returns text language plpgsql as $$ declare r text; begin select inner196() into r; return r; end $$;
select mid196();
select mid196b();
create function rethrow196() returns int language plpgsql as $$
begin
  begin
    raise exception 'boom';
  exception when others then
    raise;
  end;
  return 1;
end $$;
create function sqlwrap196() returns int language sql as $$ select 1; select rethrow196() $$;
select sqlwrap196();
create function plwrap196() returns int language plpgsql as $$ begin return sqlwrap196(); end $$;
select plwrap196();
create function plwrap196b() returns int language plpgsql as $$
begin
  begin
    return sqlwrap196();
  exception when others then
    raise notice 'caught: %', sqlstate;
    raise;
  end;
end $$;
select plwrap196b();
create function ctx196() returns text language plpgsql as $$
declare c text;
begin
  begin
    perform sqlwrap196();
  exception when others then
    get stacked diagnostics c = PG_EXCEPTION_CONTEXT;
  end;
  return c;
end $$;
select ctx196();
create function ctx196b() returns text language plpgsql as $$
declare c text;
begin
  begin
    perform sqlwrap196();
  exception when others then
    begin
      raise;
    exception when others then
      get stacked diagnostics c = PG_EXCEPTION_CONTEXT;
    end;
  end;
  return c;
end $$;
select ctx196b();

-- regression: each utility statement runs in a subcontext
-- deleted at postquel_end (functions.c:1314/1467); repeated calls through one
-- FmgrInfo do not accumulate statement copies
create function u196() returns bigint language sql as $$
  set local enable_seqscan = on;
  select sum(total_bytes) from pg_backend_memory_contexts where name like 'SQL function%' $$;
select count(distinct u196()) from generate_series(1, 300);

-- regression: the SQL function's context carries the function
-- name as its identifier (functions.c:1094)
create function m196() returns int language sql as 'select 1; select 2';
select m196();
select name, ident from pg_backend_memory_contexts where name = 'SQL function' and ident = 'm196';

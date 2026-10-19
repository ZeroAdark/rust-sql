-- regression audit XATK: a row trigger's WHEN condition is evaluated in the
-- per-tuple context (C TriggerEnabled, GetPerTupleExprContext), so a
-- by-reference intermediate is freed every row, not held to statement end.
-- Run via scripts/reg-aatk-trigger-when-memory-e2e.sh against the expected
-- captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off
create table a as select i id, 'v' || (i % 97) tx from generate_series(1, 30000) i;
create table memlog (label text, mb bigint);
create function logmem() returns trigger language plpgsql as $$ begin insert into memlog select tg_argv[0], sum(total_bytes) / 1048576 from pg_backend_memory_contexts; return null; end $$;
create function never() returns trigger language plpgsql as $$ begin raise exception 'not reached'; end $$;
create table xa (id int, tx text);
create trigger lm after insert on xa for each statement execute function logmem('after');
create trigger w after insert on xa for each row when (length(repeat(new.tx, 3000)) < 0) execute function never();
create table xb (id int, tx text);
create trigger lm after insert on xb for each statement execute function logmem('before');
create trigger w before insert on xb for each row when (length(repeat(new.tx, 3000)) < 0) execute function never();
insert into memlog select 'base', sum(total_bytes) / 1048576 from pg_backend_memory_contexts;
insert into xa select * from a;
insert into xb select * from a;
select label, mb - (select mb from memlog where label = 'base') > 50 as grew_over_50mb
  from memlog where label <> 'base' order by label;
select count(*) from xa;
select count(*) from xb;

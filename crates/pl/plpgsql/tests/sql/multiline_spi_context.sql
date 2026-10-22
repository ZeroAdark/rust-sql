\set VERBOSITY default
do $$ begin execute 'select 1/0'; end $$;
do $$ begin execute 'select
1/0'; end $$;
do $$ declare x text; begin begin execute 'select
1/0'; exception when others then get stacked diagnostics x = pg_exception_context; raise notice '%', x; end; end $$;

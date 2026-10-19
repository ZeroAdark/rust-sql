-- sequence.sql of FS-213
\set VERBOSITY verbose
\set ECHO all
-- FS-213: SET lc_time / lc_monetary / lc_numeric = '' (the empty locale). Fresh UTF8 database (datctype C), one session, superuser. PostgreSQL 18.6 warns 01000 "could not determine encoding for locale "": codeset is "ANSI_X3.4-1968"" at every use of the empty locale; pgrust never warns. Rows identical.
-- environment (lines 5-10): what the session starts from
show server_encoding;
show lc_time;
show lc_monetary;
show lc_numeric;
select name, setting, source from pg_settings where name in ('lc_time', 'lc_monetary', 'lc_numeric', 'lc_messages') order by 1;
select datname, encoding, datlocprovider, datcollate, datctype from pg_database where datname = current_database();
-- decisive 1 (lines 12-15): lc_time = '' then a TM month/day name
set lc_time = '';
show lc_time;
select to_char(date '2024-03-05', 'TMMonth TMDay');
select to_char(date '2024-03-05', 'Month Day');
-- decisive 2 (lines 17-19): lc_monetary = '' then a money literal
set lc_monetary = '';
show lc_monetary;
select '$1,000.50'::money;
-- decisive 3 (lines 21-23): lc_numeric = '' then a grouped to_char
set lc_numeric = '';
show lc_numeric;
select to_char(1234.5, '9G999D00');
-- control 1 (lines 25-30): RESET, then the same three statements: no warning on either
reset lc_time;
reset lc_monetary;
reset lc_numeric;
select to_char(date '2024-03-05', 'TMMonth TMDay');
select '$1,000.50'::money;
select to_char(1234.5, '9G999D00');
-- control 2 (lines 32-39): explicit locale names: 'C', 'POSIX', 'C.UTF-8' (if installed) are silent; 'nosuch' is 22023 on both
set lc_time = 'C';
select to_char(date '2024-03-05', 'TMMonth TMDay');
set lc_time = 'POSIX';
select to_char(date '2024-03-05', 'TMMonth TMDay');
set lc_time = 'C.UTF-8';
select to_char(date '2024-03-05', 'TMMonth TMDay');
set lc_time = 'nosuch';
show lc_time;
-- control 3 (lines 41-44): lc_messages = '' is refused 22023 on both; no warning anywhere
set lc_messages = '';
select 1/0;
reset lc_messages;
reset lc_time;
-- other forms (lines 46-57): SET LOCAL, set_config, a SQL function with SET lc_time = ''
begin; set local lc_time = ''; select to_char(date '2024-03-05', 'TMMonth'); commit;
show lc_time;
select set_config('lc_time', '', false), to_char(date '2024-03-05', 'TMMonth');
reset lc_time;
select set_config('lc_monetary', '', true), '1.5'::money;
show lc_monetary;
create function fs213_f() returns text language sql set lc_time = '' as $$ select to_char(date '2024-03-05', 'TMMonth') $$;
select fs213_f();
select fs213_f();
drop function fs213_f();
show lc_time;
select to_char(date '2024-03-05', 'TMMonth TMDay');

-- bugs/batch-289-backend-parser-parse_utilcmd: expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: USING INDEX folds the predicate like
-- RelationGetIndexPredicate; a predicate that folds to TRUE is not partial.
create table b289 (a int);
create unique index b289_i1 on b289 (a) where 1 = 1;
alter table b289 add constraint b289_c1 unique using index b289_i1;
create unique index b289_i2 on b289 (a) where (1 = 1) and (2 = 2);
alter table b289 add constraint b289_c2 unique using index b289_i2;
create unique index b289_i3 on b289 (a) where a in (1, 2, 3) or true;
alter table b289 add constraint b289_c3 primary key using index b289_i3;
create unique index b289_i4 on b289 (a) where a > 0;
alter table b289 add constraint b289_c4 unique using index b289_i4;
create unique index b289_i5 on b289 (a) where a is not null or a is null;
alter table b289 add constraint b289_c5 unique using index b289_i5;
select conname, contype, conindid::regclass from pg_constraint where conrelid = 'b289'::regclass order by conname;
select indexname, indexdef from pg_indexes where tablename = 'b289' order by indexname;
select attname, attnotnull from pg_attribute where attrelid = 'b289'::regclass and attnum > 0;
-- regression: DeconstructQualifiedName errors run outside
-- typenameType's position callback: no cursor for these, cursor for the rest.
create type b289_shell;
create table b289_t (a a.b.c.d);
create table b289_t (a a.b.c);
create table b289_t (a a.b.c.d generated always as identity);
create table b289_t (a int, b int generated always as identity, c nosuchtype);
create table b289_t of a.b.c.d;
create table b289_t of nosuchtype;
create table b289_t of nosuch.foo;
create table b289_t of b289_shell;
create table b289_t (a nosuch.int4);
create table b289_t (a nosuchtype);
create table b289_t (a int4(3));
create table b289_t (a b289_shell);
alter table b289 add column b a.b.c.d;
alter table b289 add column b a.b.c;
alter table b289 add column b nosuchtype;
alter table b289 add column b nosuch.int4;
alter table b289 add column b b289_shell;
alter table b289 add column b int4(3);
-- regression: pts_error_callback adds CONTEXT only; the inner
-- scanner's cursor is kept as-is (parse_type.c:719).
set standard_conforming_strings = off;
select to_regtype($$text('\n')$$);
select to_regtype($$"text"('\n')$$);
select to_regtype($$bit('\\')$$);
select to_regtype('varchar(''\n'')');
select to_regtype('varchar(''\\''  ,  ''\\'')');
reset standard_conforming_strings;
select to_regtype('varchar(E''\n'')');
-- regression: the position callback spans the TYPENAMENSP lookup
-- (parse_type.c:177-187), so a lock timeout inside it is positioned.
\setenv PGHOST :HOST
\setenv PGPORT :PORT
\setenv PGDATABASE :DBNAME
\setenv PGUSER :USER
\! "${PGINSTALL:-$(dirname "$(command -v psql)")}/psql" -X -q -c "begin; lock table pg_catalog.pg_type in access exclusive mode; select pg_sleep(4);" >/dev/null 2>&1 &
select pg_sleep(1.5);
set lock_timeout = '200ms';
select 1::pg_catalog.b289_nosuch_type;
select 1::b289_nosuch_type;
create function b289_f(pg_catalog.b289_nosuch_type) returns int language sql as 'select 1';
reset lock_timeout;
select pg_sleep(3);
drop table b289;
drop type b289_shell;

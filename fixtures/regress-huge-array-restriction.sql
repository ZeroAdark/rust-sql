-- A restriction clause against a multi-million-element constant array is
-- estimated element by element.
create table wio509 (a int);
insert into wio509 values (7), (8);
select count(*) as n_5m from wio509 where a = any (string_to_array(repeat('7,', 4999999) || '7', ',')::int[]);
select count(*) as n_5m_ne_all from wio509 where a <> all (string_to_array(repeat('7,', 4999999) || '7', ',')::int[]);
select count(*) as n_small from wio509 where a = any (array[1, 7, 9]);
select count(*) as n_arrayexpr from wio509 where a = any (array[1, a - 1, 9]);

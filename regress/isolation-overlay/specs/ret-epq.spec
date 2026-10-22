# M4-RET R1 (D682's RETURNING half): DELETE / UPDATE .. RETURNING whose scan meets a row a concurrent UPDATE moved out of
# its qual (or kept in it): the rows after the rechecked one are still written and returned, as PostgreSQL does; plain
# DELETE, a STORED generated column, FOR UPDATE and a cursor beside it; and plpgsql DELETE / UPDATE .. RETURNING INTO (SPI
# asks for two rows, tcount 2) with the recheck before or after the returned row. M4-RET R4a: UPDATE / DELETE ..
# RETURNING old / new (PG 18) across a recheck after a concurrent UPDATE of the SET column or of another column: OLD is
# the version the statement replaced (the one the recheck locked), never the one its snapshot saw.
setup
{
 CREATE TABLE acc (id text PRIMARY KEY, bal numeric not null);
 INSERT INTO acc VALUES ('checking', 600), ('savings', 600);
 CREATE TABLE accg (id text PRIMARY KEY, bal numeric not null, bal2 numeric GENERATED ALWAYS AS (bal * 2) STORED);
 INSERT INTO accg VALUES ('checking', 600), ('savings', 600);
 CREATE TABLE acc3 (id text PRIMARY KEY, bal numeric not null);
 INSERT INTO acc3 VALUES ('a', 600), ('checking', 600), ('z', 600);
 CREATE TABLE acco (id text PRIMARY KEY, bal numeric not null, note text);
 INSERT INTO acco VALUES ('a', 600, 'a0'), ('checking', 600, 'c0'), ('z', 600, 'z0');
 CREATE FUNCTION del_into(t text) RETURNS text LANGUAGE plpgsql AS $$
   DECLARE x text; n bigint;
   BEGIN EXECUTE format('DELETE FROM %I WHERE bal = 600 RETURNING id', t) INTO x; GET DIAGNOSTICS n = ROW_COUNT;
     RETURN x || ' ' || n; END $$;
 CREATE FUNCTION del_into_acc() RETURNS text LANGUAGE plpgsql AS $$
   DECLARE x text; n bigint;
   BEGIN DELETE FROM acc WHERE bal = 600 RETURNING id INTO x; GET DIAGNOSTICS n = ROW_COUNT; RETURN x || ' ' || n; END $$;
 CREATE FUNCTION upd_into(t text) RETURNS text LANGUAGE plpgsql AS $$
   DECLARE x text; n bigint;
   BEGIN EXECUTE format('UPDATE %I SET bal = bal + 1 WHERE bal = 600 RETURNING id', t) INTO x; GET DIAGNOSTICS n = ROW_COUNT;
     RETURN x || ' ' || n; END $$;
}
teardown { DROP FUNCTION upd_into(text); DROP FUNCTION del_into(text); DROP FUNCTION del_into_acc(); DROP TABLE acc; DROP TABLE accg; DROP TABLE acc3; DROP TABLE acco; }
session s1
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step dr { DELETE FROM acc WHERE bal = 600 RETURNING *; }
step dn { DELETE FROM acc WHERE bal = 600; }
step dg { DELETE FROM accg WHERE bal = 600 RETURNING *; }
step d3 { DELETE FROM acc3 WHERE bal = 600 RETURNING id; }
step sf { SELECT * FROM acc WHERE bal = 600 FOR UPDATE; }
step cf { DECLARE c CURSOR FOR SELECT * FROM acc WHERE bal = 600 FOR UPDATE; FETCH ALL FROM c; }
step di { SELECT del_into_acc(); }
step di3 { SELECT del_into('acc3'); }
step ur { UPDATE acc SET bal = bal + 1 WHERE bal = 600 RETURNING *; }
step ug { UPDATE accg SET bal = bal + 1 WHERE bal = 600 RETURNING id, bal; }
step u3 { UPDATE acc3 SET bal = bal + 1 WHERE bal = 600 RETURNING id, bal; }
step ui3 { SELECT upd_into('acc3'); }
step uo { UPDATE acco SET bal = bal + 1 WHERE bal >= 600 RETURNING id, old.bal AS ob, new.bal AS nb, old.note AS onote, new.note AS nnote; }
step dol { DELETE FROM acco WHERE bal >= 600 RETURNING id, old.bal AS ob, new.bal AS nb, old.note AS onote; }
step c1 { COMMIT; }
session s2
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step wx { UPDATE acc SET bal = bal + 450 WHERE id = 'checking'; }
step wg { UPDATE accg SET bal = bal + 450 WHERE id = 'checking'; }
step wk { UPDATE acc SET id = 'checking' WHERE id = 'checking'; }
step w3 { UPDATE acc3 SET bal = bal + 450 WHERE id = 'checking'; }
step w3a { UPDATE acc3 SET bal = bal + 450 WHERE id IN ('checking', 'z'); }
step wob { UPDATE acco SET bal = bal + 5 WHERE id = 'checking'; }
step won { UPDATE acco SET note = 'c2' WHERE id = 'checking'; }
step wox { UPDATE acco SET bal = 1 WHERE id = 'checking'; }
step c2 { COMMIT; }
session s3
step read { SELECT * FROM acc ORDER BY id; }
step readg { SELECT * FROM accg ORDER BY id; }
step read3 { SELECT * FROM acc3 ORDER BY id; }
step reado { SELECT * FROM acco ORDER BY id; }
permutation read wx dr c2 c1 read
permutation read wx dn c2 c1 read
permutation readg wg dg c2 c1 readg
permutation read wk dr c2 c1 read
permutation read3 w3 d3 c2 c1 read3
permutation read wx sf c2 c1 read
permutation read wx cf c2 c1 read
permutation read wx di c2 c1 read
permutation read wk di c2 c1 read
permutation read3 w3a di3 c2 c1 read3
permutation read3 w3 di3 c2 c1 read3
permutation read wx ur c2 c1 read
permutation read wk ur c2 c1 read
permutation readg wg ug c2 c1 readg
permutation read3 w3 u3 c2 c1 read3
permutation read3 w3 ui3 c2 c1 read3
permutation reado wob uo c2 c1 reado
permutation reado won uo c2 c1 reado
permutation reado wox uo c2 c1 reado
permutation reado won dol c2 c1 reado

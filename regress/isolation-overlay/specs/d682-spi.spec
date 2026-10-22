# D682: a write the entry seam serves (an SPI DELETE / UPDATE in a plpgsql function) whose first row is concurrently
# updated out of (or kept in) its qual: the rows after the rechecked one are still written, the count is PostgreSQL's
setup
{
 CREATE TABLE acc (id text PRIMARY KEY, bal numeric not null);
 INSERT INTO acc VALUES ('checking', 600), ('savings', 600);
 CREATE FUNCTION del600() RETURNS bigint LANGUAGE plpgsql AS $$
   DECLARE n bigint; BEGIN DELETE FROM acc WHERE bal = 600; GET DIAGNOSTICS n = ROW_COUNT; RETURN n; END $$;
 CREATE FUNCTION upd600() RETURNS bigint LANGUAGE plpgsql AS $$
   DECLARE n bigint; BEGIN UPDATE acc SET bal = bal + 1 WHERE bal = 600; GET DIAGNOSTICS n = ROW_COUNT; RETURN n; END $$;
}
teardown { DROP FUNCTION del600(); DROP FUNCTION upd600(); DROP TABLE acc; }
session s1
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step dfn { SELECT del600(); }
step ufn { SELECT upd600(); }
step c1 { COMMIT; }
session s2
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step wx { UPDATE acc SET bal = bal + 450 WHERE id = 'checking'; }
step wk { UPDATE acc SET id = 'checking' WHERE id = 'checking'; }
step c2 { COMMIT; }
session s3
step read { SELECT * FROM acc ORDER BY id; }
permutation read wx dfn c2 c1 read
permutation read wk dfn c2 c1 read
permutation read wx ufn c2 c1 read
permutation read wk ufn c2 c1 read

# D3775 COPYPAIR (SQLAlchemy's joinedload with a LIMIT and with_for_update): a LIMIT 1 FOR UPDATE subquery joined under
# the statement's FOR UPDATE, the subquery a Hash Join's build (LockRows -> Hash Join (Seq Scan a, Hash -> Subquery Scan
# anon_1 -> Limit -> LockRows -> Seq Scan u)). Each LockRows has its own ExecRowMarks and EvalPlanQual: the inner
# rechecks u's newest version under its LIMIT (a deleted row skipped, the next one taken); the outer refetches a's newest
# version and reads anon_1's row stored (ROW_MARK_COPY), never running the inner again. NOWAIT and SKIP LOCKED apply to
# each lock's own relation. An empty probe side never builds the hash, so the inner never waits. Expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3775u (id serial PRIMARY KEY, name varchar(30));
  CREATE TABLE d3775a (id serial PRIMARY KEY, user_id int REFERENCES d3775u, email_address varchar(50));
  INSERT INTO d3775u (name) VALUES ('jack'), ('ed'), ('fred'), ('chuck');
  INSERT INTO d3775a (user_id, email_address) VALUES (1, 'jack@bean.com'), (2, 'ed@wood.com'), (2, 'ed@bettyboop.com'), (2, 'ed@lala.com'), (3, 'fred@fred.com');
}
teardown { DROP TABLE d3775a, d3775u; }

session s1
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s1q	{ SELECT anon_1.users_id, anon_1.users_name, addresses_1.id, addresses_1.email_address FROM (SELECT d3775u.id AS users_id, d3775u.name AS users_name FROM d3775u LIMIT 1 FOR UPDATE) AS anon_1 JOIN d3775a AS addresses_1 ON anon_1.users_id = addresses_1.user_id FOR UPDATE; }
step s1nw	{ SELECT anon_1.users_id, addresses_1.id FROM (SELECT d3775u.id AS users_id FROM d3775u LIMIT 1 FOR UPDATE NOWAIT) AS anon_1 JOIN d3775a AS addresses_1 ON anon_1.users_id = addresses_1.user_id FOR UPDATE NOWAIT; }
step s1sk	{ SELECT anon_1.users_id, addresses_1.id FROM (SELECT d3775u.id AS users_id FROM d3775u LIMIT 1 FOR UPDATE SKIP LOCKED) AS anon_1 JOIN d3775a AS addresses_1 ON anon_1.users_id = addresses_1.user_id FOR UPDATE SKIP LOCKED; }
step s1e	{ SELECT anon_1.users_id, addresses_1.id FROM (SELECT d3775u.id AS users_id FROM d3775u LIMIT 1 FOR UPDATE) AS anon_1 JOIN d3775a AS addresses_1 ON anon_1.users_id = addresses_1.user_id WHERE addresses_1.email_address = 'nobody' FOR UPDATE; }

session s2
step s2b	{ BEGIN; }
step s2un	{ UPDATE d3775u SET name = 'jack2' WHERE id = 1; }
step s2ud	{ DELETE FROM d3775a WHERE user_id = 1; DELETE FROM d3775u WHERE id = 1; }
step s2ae	{ UPDATE d3775a SET email_address = 'jack@new.com' WHERE id = 1; }
step s2am	{ UPDATE d3775a SET user_id = 3 WHERE id = 1; }
step s2lu	{ SELECT id FROM d3775u WHERE id = 1 FOR UPDATE; }
step s2la	{ SELECT id FROM d3775a WHERE id = 1 FOR UPDATE; }
step s2c	{ COMMIT; }

# the inner's EvalPlanQual: u 1's newest name; u 1 deleted, the LIMIT takes u 2
permutation s2b s2un s1q s2c
permutation s2b s2ud s1q s2c
# the outer's EvalPlanQual over anon_1's stored row: a 1's newest email; a 1 moved to user 3 no longer joins
permutation s2b s2ae s1q s2c
permutation s2b s2am s1q s2c
# NOWAIT on each lock's relation
permutation s2b s2lu s1nw s2c
permutation s2b s2la s1nw s2c
# SKIP LOCKED: the inner takes u 2; the outer skips a 1
permutation s2b s2lu s1sk s2c
permutation s2b s2la s1sk s2c
# an empty probe side: the hash is never built, the inner never waits on u 1
permutation s2b s2lu s1e s2c

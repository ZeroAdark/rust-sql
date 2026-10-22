# D652-H (modifytable): a DELETE / UPDATE whose WHERE holds a hashed SubPlan (an uncorrelated NOT IN) rechecks in the
# IR. s2 waits on the target rows s1 changed; after s1 commits, EvalPlanQual rechecks each row's newest version against
# the same table (PostgreSQL rebuilds it in the EPQ estate under the statement's snapshot, so s1's new product row is
# not in it, and a row s1 points at it still goes). A row whose product_id s1 moved into the list is kept, one s1
# changed in another column goes, a row s1 deleted is skipped, and a row in the list that s1 moved out was not in s2's
# result and is never rechecked. With s2 first, s1 waits on s2; after a rollback of s1, s2 writes the old versions.

setup
{
 CREATE TABLE hp (id int PRIMARY KEY, discontinued bool NOT NULL);
 INSERT INTO hp SELECT g, g % 5 = 0 FROM generate_series(1, 400) g;
 CREATE TABLE hi (id int PRIMARY KEY, product_id int, qty int NOT NULL);
 INSERT INTO hi SELECT g, CASE WHEN g BETWEEN 401 AND 405 THEN g - 400 ELSE g END, g % 7 FROM generate_series(1, 440) g;
 ANALYZE hp, hi;
}

teardown
{
 DROP TABLE hi;
 DROP TABLE hp;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1p	{ INSERT INTO hp VALUES (431, false); }
step s1in	{ UPDATE hi SET product_id = 7 WHERE id = 421; }
step s1q	{ UPDATE hi SET qty = 100 WHERE id IN (425, 10); }
step s1d	{ DELETE FROM hi WHERE id = 428; }
step s1out	{ UPDATE hi SET product_id = 999 WHERE id = 402; }
step s1nw	{ UPDATE hi SET product_id = 431 WHERE id = 431; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ DELETE FROM hi WHERE product_id NOT IN (SELECT id FROM hp WHERE NOT discontinued) AND id > 400 RETURNING id, product_id, qty; }
step s2u	{ UPDATE hi SET qty = -qty WHERE product_id NOT IN (SELECT id FROM hp WHERE NOT discontinued) AND id > 400 RETURNING id, product_id, qty; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, product_id, qty FROM hi WHERE id IN (10, 402, 405, 421, 425, 428, 431, 440) ORDER BY id; }
step s3cnt	{ SELECT count(*), sum(qty) FROM hi WHERE id > 400; }

permutation s1p s1in s1q s1d s1out s1nw s2d s1c s2c s3sel s3cnt
permutation s1in s1q s1d s2u s1c s2c s3sel s3cnt
permutation s2d s1in s2c s1q s1c s3sel s3cnt
permutation s1in s1q s1d s2d s1r s2c s3sel s3cnt

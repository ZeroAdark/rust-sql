# M6-HOSTED (main 09-28): a hashed group over a composite key (rep.hosted.hash's canonical form) while another session
# holds an uncommitted ALTER TYPE .. ADD ATTRIBUTE: each session's group reads its own catalog view (the lowering's form,
# interned by content, never another session's), before and after the ALTER commits or rolls back. The group's shape
# (its count, rows and largest group) is what each session must match: s1's uncommitted UPDATE splits groups s2 never sees.
setup
{
 CREATE TYPE m6ap AS (a int, b text);
 CREATE TABLE m6apt (g int NOT NULL, c m6ap);
 INSERT INTO m6apt SELECT i, ROW(i % 4, 'b' || (i % 3))::m6ap FROM generate_series(1, 1200) i;
 INSERT INTO m6apt VALUES (0, NULL), (-1, ROW(NULL, NULL)::m6ap);
}
teardown { DROP TABLE m6apt; DROP TYPE m6ap; }
session s1
step alt { BEGIN; ALTER TYPE m6ap ADD ATTRIBUTE z int; UPDATE m6apt SET c = ROW((c).a, (c).b, g % 3)::m6ap WHERE g % 5 = 0; }
step g1 { SELECT count(*) AS groups, sum(n) AS rows, max(n) AS biggest FROM (SELECT c, count(*) AS n FROM m6apt GROUP BY c) s; }
step rb { ROLLBACK; }
step cm { COMMIT; }
session s2
step g2 { SELECT count(*) AS groups, sum(n) AS rows, max(n) AS biggest FROM (SELECT c, count(*) AS n FROM m6apt GROUP BY c) s; }
permutation g2 alt g1 g2 rb g2
permutation alt g1 g2 cm g2 g1

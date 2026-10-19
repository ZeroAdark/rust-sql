-- bugs/batch-213-backend-utils-adt: utils/adt fixes, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: CYCLE marks coerced to a common type are FuncExprs,
-- not Consts (ruleutils.c:5875 castNode is a plain cast in release builds);
-- pgrust panicked in get_with_clause.
CREATE VIEW b213_cyc AS WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n < 3) CYCLE n SET c TO 1 DEFAULT 0.5 USING p SELECT n FROM t;
SELECT pg_get_viewdef('b213_cyc'::regclass);
SELECT pg_get_viewdef('b213_cyc'::regclass, true);
SELECT * FROM b213_cyc;
CREATE VIEW b213_cyc2 AS WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n < 3) CYCLE n SET c TO true DEFAULT false USING p SELECT n FROM t;
SELECT pg_get_viewdef('b213_cyc2'::regclass);
DROP VIEW b213_cyc, b213_cyc2;
-- regression: hypothetical dense_rank compares adjacent rows
-- under a per-tuple context (orderedsetaggs.c:1408 ExecQualAndReset);
-- record/array/toasted-text keys panicked on the unarmed result context.
SELECT dense_rank((2,2)) WITHIN GROUP (ORDER BY (a,b)) FROM (VALUES (1,1),(1,2),(3,3)) v(a,b);
SELECT dense_rank((1,2)) WITHIN GROUP (ORDER BY (a,b)) FROM (VALUES (1,1),(1,2),(1,2),(3,3)) v(a,b);
SELECT dense_rank(ARRAY[5]) WITHIN GROUP (ORDER BY a) FROM (VALUES (ARRAY[1]),(ARRAY[2]),(ARRAY[2]),(ARRAY[9])) v(a);
SELECT dense_rank('m'::text) WITHIN GROUP (ORDER BY t) FROM (SELECT repeat(x, 3000) AS t FROM (VALUES ('a'),('a'),('b'),('z')) v(x)) s;
SELECT dense_rank(2, 'm'::text) WITHIN GROUP (ORDER BY a, t) FROM (SELECT a, repeat(x, 3000) AS t FROM (VALUES (1,'a'),(1,'a'),(2,'b'),(3,'z')) v(a,x)) s;

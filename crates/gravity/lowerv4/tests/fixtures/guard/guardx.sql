-- GUARD-G1's guardx set (design REV 2; the kit probe-kit-user-fn K1 / K2 / K3 /
-- K6 forms and the S1 box key): run over a fresh initdb by
-- `OUT=crates/gravity/lowerv4/tests/fixtures/guard scripts/gravity-capture-plans.sh guardx`; the fixtures of the tagged
-- statements (`/* G.<err|ok>.<form> */`) are kept, tests/guard_user_fn.rs walks each through the guard (admit_all): an `err`
-- statement reaches a support function that is not built in (or, while DISPATCH_OK is false, a DISPATCH one), an `ok` one
-- does not. The objects are the kit's (K1 k1_ops / k1_box_ops, K2 k2r, K3 k3_tn / k3_thn, K6 k6r / int4range) plus the S1
-- box key as a DEFAULT btree opclass (a sorted SetOp, MergeAppend and Incremental Sort over it) and the controls.
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE SEQUENCE gq;
CREATE FUNCTION g_ncmp(int4, int4) RETURNS int4 LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN PERFORM nextval('gq'); RETURN btint4cmp($1, $2); END $$;
CREATE OPERATOR <^< (leftarg = int4, rightarg = int4, function = int4lt);
CREATE OPERATOR <^<= (leftarg = int4, rightarg = int4, function = int4le);
CREATE OPERATOR =^= (leftarg = int4, rightarg = int4, function = int4eq, commutator = =^=, restrict = eqsel, join = eqjoinsel, MERGES);
CREATE OPERATOR >^>= (leftarg = int4, rightarg = int4, function = int4ge);
CREATE OPERATOR >^> (leftarg = int4, rightarg = int4, function = int4gt);
CREATE OPERATOR CLASS k1_ops FOR TYPE int4 USING btree AS OPERATOR 1 <^<, OPERATOR 2 <^<=, OPERATOR 3 =^=, OPERATOR 4 >^>=, OPERATOR 5 >^>, FUNCTION 1 g_ncmp(int4, int4);
CREATE OPERATOR CLASS k2_nops FOR TYPE int4 USING btree AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 g_ncmp(int4, int4);
CREATE FUNCTION g_boxcmp(box, box) RETURNS int4 LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN PERFORM nextval('gq'); RETURN CASE WHEN area($1) < area($2) THEN -1 WHEN area($1) > area($2) THEN 1 ELSE 0 END; END $$;
CREATE OPERATOR CLASS g_box_ops DEFAULT FOR TYPE box USING btree AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 g_boxcmp(box, box);
CREATE TYPE k2r AS RANGE (subtype = int4, subtype_opclass = k2_nops);
CREATE FUNCTION g_nhash(int4, int8) RETURNS int8 LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN PERFORM nextval('gq'); RETURN hashint4extended($1, $2); END $$;
CREATE OPERATOR CLASS k3_nhash FOR TYPE int4 USING hash AS OPERATOR 1 =, FUNCTION 2 g_nhash(int4, int8);
CREATE TYPE g_en AS ENUM ('b', 'a', 'c');
CREATE DOMAIN g_dom AS int4 CHECK (VALUE > 0);
CREATE TABLE k1t (id int, g int, x int, t text, e g_en, d g_dom);
INSERT INTO k1t SELECT i, i % 4, (i * 37) % 101, 'v' || (i % 9), (ARRAY['a', 'b', 'c']::g_en[])[1 + i % 3], 1 + i % 5 FROM generate_series(1, 60) i;
CREATE TABLE k1u (id int, x int);
INSERT INTO k1u SELECT i, (i * 29) % 101 FROM generate_series(1, 40) i;
CREATE TABLE gbx1 (k int, b box);
CREATE TABLE gbx2 (k int, b box);
INSERT INTO gbx1 SELECT i % 50, box(point(0, 0), point((i * 7919) % 3001, 1)) FROM generate_series(1, 2000) i;
INSERT INTO gbx2 SELECT i % 50, box(point(0, 0), point((i * 104729) % 3001, 1)) FROM generate_series(1, 2000) i;
CREATE INDEX gbx1_b ON gbx1 (b);
CREATE INDEX gbx2_b ON gbx2 (b);
CREATE INDEX gbx1_k ON gbx1 (k);
CREATE TABLE k2t (id int, r k2r);
INSERT INTO k2t SELECT i, k2r(i % 11, i % 11 + 1 + i % 5) FROM generate_series(1, 60) i;
CREATE TABLE k2u (id int, r k2r);
INSERT INTO k2u SELECT i, k2r(i % 7, i % 7 + 3) FROM generate_series(1, 30) i;
CREATE TABLE k6b (id int, rn int4range, ia int4[], fa real[]);
INSERT INTO k6b SELECT i, int4range(i % 11, i % 11 + 1 + i % 5), ARRAY[i % 3, i % 5], ARRAY[(i % 4)::real] FROM generate_series(1, 60) i;
CREATE TABLE k3_tn (c int) PARTITION BY RANGE (c k2_nops);
CREATE TABLE k3_tn_1 PARTITION OF k3_tn FOR VALUES FROM (1) TO (501);
CREATE TABLE k3_tn_2 PARTITION OF k3_tn FOR VALUES FROM (501) TO (5001);
CREATE TABLE k3_thn (c int) PARTITION BY HASH (c k3_nhash);
CREATE TABLE k3_thn_0 PARTITION OF k3_thn FOR VALUES WITH (MODULUS 2, REMAINDER 0);
CREATE TABLE k3_thn_1 PARTITION OF k3_thn FOR VALUES WITH (MODULUS 2, REMAINDER 1);
CREATE TABLE k3_tq (c int) PARTITION BY RANGE (c);
CREATE TABLE k3_tq_1 PARTITION OF k3_tq FOR VALUES FROM (1) TO (501);
CREATE TABLE k3_tq_2 PARTITION OF k3_tq FOR VALUES FROM (501) TO (5001);
INSERT INTO k3_tn VALUES (10), (20), (501), (4500);
INSERT INTO k3_thn SELECT i FROM generate_series(1, 30) i;
INSERT INTO k3_tq VALUES (10), (20), (501), (4500);
ANALYZE k1t, k1u, gbx1, gbx2, k2t, k2u, k6b, k3_tn, k3_thn, k3_tq;

-- K1: a user btree opclass (FUNCTION 1 plpgsql): its sort, merge key and ordered aggregate; the box trap (D1371-R6 S2)
SELECT id, x FROM k1t ORDER BY x USING <^< /* G.err.k1_sort */;
SELECT array_agg(x ORDER BY x USING <^<) FROM k1t /* G.err.k1_aggorder */;
SET enable_hashjoin = off; SET enable_nestloop = off;
SELECT count(*) FROM k1t a JOIN k1u b ON a.x =^= b.x /* G.err.k1_merge */;
SELECT count(*) FROM k1t a JOIN k1u b ON a.x = b.x /* G.ok.int4_merge */;
RESET enable_hashjoin; RESET enable_nestloop;
SELECT b FROM gbx1 ORDER BY b USING < /* G.err.k1_box */;
-- S1: the box key's sorted SetOp, MergeAppend and Incremental Sort (no Sort node of their own compares it)
SET enable_hashagg = off;
SELECT b FROM gbx1 INTERSECT SELECT b FROM gbx2 /* G.err.s1_setop */;
RESET enable_hashagg;
SET enable_sort = off;
SELECT b FROM (SELECT b FROM gbx1 UNION ALL SELECT b FROM gbx2) u ORDER BY b /* G.err.s1_mergeappend */;
RESET enable_sort;
SELECT k, b FROM gbx1 ORDER BY k, b LIMIT 5 /* G.err.s1_incsort */;
-- K2 / K6: a user range (range_cmp / hash_range reach its subtype opclass's FUNCTION 1)
SELECT r FROM k2t ORDER BY r /* G.err.k2_sort */;
SET enable_mergejoin = off; SET enable_nestloop = off;
SELECT count(*) FROM k2t a JOIN k2u b ON a.r = b.r /* G.err.k2_hashjoin */;
RESET enable_mergejoin; RESET enable_nestloop;
SET enable_hashagg = off;
SELECT r, count(*) FROM k2t GROUP BY r /* G.err.k6_group */;
-- S4 / D2093: while DISPATCH_OK is false a built-in range or array key refuses too
SELECT rn, count(*) FROM k6b GROUP BY rn /* G.err.dispatch_int4range */;
RESET enable_hashagg;
SELECT ia FROM k6b ORDER BY ia /* G.err.dispatch_int4arr */;
SELECT fa FROM k6b ORDER BY fa /* G.err.dispatch_realarr */;
-- K3: run-time pruning over a user partition opclass (btree FUNCTION 1, hash FUNCTION 2)
SELECT * FROM k3_tn WHERE c = (SELECT 20) /* G.err.k3_prune */;
SELECT * FROM k3_thn WHERE c = (SELECT 20) /* G.err.k3_prune_hash */;
-- controls: built-in int4 / text keys, an enum, a domain, int4 < in a user family beside integer_ops (T2)
SELECT id, x FROM k1t ORDER BY x /* G.ok.int4_sort */;
SELECT t, count(*) FROM k1t GROUP BY t /* G.ok.text_hashagg */;
SELECT t FROM k1t ORDER BY t /* G.ok.text_sort */;
SELECT e, count(*) FROM k1t GROUP BY e ORDER BY e /* G.ok.enum_group */;
SELECT d FROM k1t ORDER BY d /* G.ok.domain_sort */;
SELECT count(*) FROM k1t a JOIN k1u b ON a.x = b.x /* G.ok.int4_hashjoin */;
SELECT x FROM k1t INTERSECT SELECT x FROM k1u /* G.ok.int4_setop */;
SELECT x, sum(id) OVER (PARTITION BY g ORDER BY x) FROM k1t /* G.ok.int4_window */;
SELECT * FROM k1t WHERE x NOT IN (SELECT x FROM k1u) /* G.ok.hashed_subplan */;
SELECT * FROM k3_tq WHERE c = (SELECT 20) /* G.ok.prune_builtin */;
SELECT count(DISTINCT x) FROM k1t /* G.ok.int4_distinct_agg */;
-- an ordered-set / hypothetical-set aggregate compares by its clause's eqop (mode(), dense_rank() WITHIN GROUP): a user =
-- in a btree family whose FUNCTION 1 is built in (review round 1, F1); mode() over int4's own family is the control
CREATE FUNCTION g_mod_eq(int4, int4) RETURNS bool LANGUAGE plpgsql IMMUTABLE STRICT AS $$ BEGIN PERFORM nextval('gq'); RETURN $1 % 10 = $2 % 10; END $$;
CREATE OPERATOR <<< (leftarg = int4, rightarg = int4, function = int4lt);
CREATE OPERATOR <<= (leftarg = int4, rightarg = int4, function = int4le);
CREATE OPERATOR === (leftarg = int4, rightarg = int4, function = g_mod_eq);
CREATE OPERATOR >>= (leftarg = int4, rightarg = int4, function = int4ge);
CREATE OPERATOR >>> (leftarg = int4, rightarg = int4, function = int4gt);
CREATE OPERATOR CLASS g_mod_ops FOR TYPE int4 USING btree AS OPERATOR 1 <<<, OPERATOR 2 <<=, OPERATOR 3 ===, OPERATOR 4 >>=, OPERATOR 5 >>>, FUNCTION 1 btint4cmp(int4, int4);
SELECT mode() WITHIN GROUP (ORDER BY x USING <<<) FROM k1t /* G.err.osa_mode_eqop */;
SELECT dense_rank(5) WITHIN GROUP (ORDER BY x USING <<<) FROM k1t /* G.err.hsa_dense_rank_eqop */;
SELECT mode() WITHIN GROUP (ORDER BY x) FROM k1t /* G.ok.osa_mode_int4 */;

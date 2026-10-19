CREATE EXTENSION intarray;
CREATE EXTENSION pageinspect;

-- g_intbig_picksplit orders equal split costs with pg_qsort, which decides
-- the membership of the two halves. Only the first split is free of
-- gistchoose's random tie-breaking, so insert singletons until the root leaf
-- splits once and compare the resulting pages.
CREATE TABLE picksplit_big(a int[]);
CREATE INDEX picksplit_big_idx ON picksplit_big USING gist(a gist__intbig_ops);
DO $$
BEGIN
  FOR g IN 1..5000 LOOP
    INSERT INTO picksplit_big VALUES (ARRAY[g]);
    EXIT WHEN pg_relation_size('picksplit_big_idx') > 8192;
  END LOOP;
END $$;
SELECT count(*) AS rows_until_split FROM picksplit_big;
SELECT pg_relation_size('picksplit_big_idx') / 8192 AS blocks;
SELECT b, count(*), min(ctid::text), max(ctid::text)
FROM generate_series(0, (pg_relation_size('picksplit_big_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_big_idx', b))
GROUP BY b ORDER BY b;
SELECT b, itemoffset, ctid, key_data
FROM generate_series(1, (pg_relation_size('picksplit_big_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_big_idx', b))
ORDER BY b, itemoffset;
DROP TABLE picksplit_big;

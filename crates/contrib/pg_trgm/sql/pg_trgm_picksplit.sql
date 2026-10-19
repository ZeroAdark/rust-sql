CREATE EXTENSION pg_trgm;
CREATE EXTENSION pageinspect;

-- gtrgm_picksplit orders equal split costs with pg_qsort, which decides the
-- membership of the two halves. Only the first split is free of gistchoose's
-- random tie-breaking, so insert rows until the root leaf splits once and
-- compare the resulting pages.
CREATE TABLE picksplit_t(s text);
CREATE INDEX picksplit_idx ON picksplit_t USING gist(s gist_trgm_ops);
DO $$
BEGIN
  FOR g IN 1..2000 LOOP
    INSERT INTO picksplit_t VALUES (md5(g::text));
    EXIT WHEN pg_relation_size('picksplit_idx') > 8192;
  END LOOP;
END $$;
SELECT count(*) AS rows_until_split FROM picksplit_t;
SELECT pg_relation_size('picksplit_idx') / 8192 AS blocks;
SELECT b, count(*), md5(string_agg(md5(key_data::text), ',' ORDER BY itemoffset)) AS page_keys
FROM generate_series(0, (pg_relation_size('picksplit_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_idx', b))
GROUP BY b ORDER BY b;
SELECT b, itemoffset, ctid, itemlen
FROM generate_series(1, (pg_relation_size('picksplit_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_idx', b))
ORDER BY b, itemoffset;
DROP TABLE picksplit_t;

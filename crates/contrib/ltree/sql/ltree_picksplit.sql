CREATE EXTENSION ltree;
CREATE EXTENSION pageinspect;

-- ltree_picksplit sorts the entries with pg_qsort (treekey_cmp) and cuts the
-- sorted list in half; _ltree_picksplit orders equal split costs with
-- pg_qsort. Equal keys / equal costs land in the half pg_qsort's order gives
-- them. Only the first split is free of gistchoose's random tie-breaking, so
-- insert until the root leaf splits once and compare the resulting pages.
CREATE TABLE picksplit_lt(p ltree);
CREATE INDEX picksplit_lt_idx ON picksplit_lt USING gist(p);
DO $$
BEGIN
  FOR g IN 1..5000 LOOP
    INSERT INTO picksplit_lt VALUES (('k' || (g % 7))::ltree);
    EXIT WHEN pg_relation_size('picksplit_lt_idx') > 8192;
  END LOOP;
END $$;
SELECT count(*) AS rows_until_split FROM picksplit_lt;
SELECT pg_relation_size('picksplit_lt_idx') / 8192 AS blocks;
SELECT b, count(*), min(ctid::text), max(ctid::text)
FROM generate_series(0, (pg_relation_size('picksplit_lt_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_lt_idx', b))
GROUP BY b ORDER BY b;
SELECT b, itemoffset, ctid, key_data
FROM generate_series(1, (pg_relation_size('picksplit_lt_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_lt_idx', b))
ORDER BY b, itemoffset;
DROP TABLE picksplit_lt;

CREATE TABLE picksplit_alt(p ltree[]);
CREATE INDEX picksplit_alt_idx ON picksplit_alt USING gist(p);
DO $$
BEGIN
  FOR g IN 1..5000 LOOP
    INSERT INTO picksplit_alt VALUES (ARRAY[('n' || g)::ltree]);
    EXIT WHEN pg_relation_size('picksplit_alt_idx') > 8192;
  END LOOP;
END $$;
SELECT count(*) AS rows_until_split FROM picksplit_alt;
SELECT pg_relation_size('picksplit_alt_idx') / 8192 AS blocks;
SELECT b, count(*), min(ctid::text), max(ctid::text)
FROM generate_series(0, (pg_relation_size('picksplit_alt_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_alt_idx', b))
GROUP BY b ORDER BY b;
SELECT b, itemoffset, ctid, key_data
FROM generate_series(1, (pg_relation_size('picksplit_alt_idx') / 8192)::int - 1) b,
     LATERAL gist_page_items_bytea(get_raw_page('picksplit_alt_idx', b))
ORDER BY b, itemoffset;
DROP TABLE picksplit_alt;

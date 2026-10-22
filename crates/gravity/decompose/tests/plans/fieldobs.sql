-- The fieldobs corpus: statements over the neutral fieldobs dataset (fixtures/neutral/fieldobs.sql: a field-survey log,
-- sightings columnar sorted_by (site_id, observed_on, observed_at) with COLLATE "C" text, its heap twin sightings_heap,
-- observers columnar with default-collation text). Each statement is here for the decomposer / lowering feature its
-- comment names: plain folds and the answers a part's statistics give, distinct counts, flat filtered scans, grouped
-- top-k by count (owner.count, the code fronts, the group walk), row top-k (the row walk, late fetch, text sort keys),
-- HAVING / expression keys and the sorted-prefix verdicts (the text-key heap twins are bench.sql's SV statements).
-- Captured by scripts/gravity-capture-plans.sh fieldobs over a fresh initdb: the dataset loads at :rows (default
-- 10,000,000: planner estimates of a large table) with plan logging off, then every EXPLAIN becomes fieldobs-<nnn>.plan
-- in statement order.
SET debug_print_plan = off;
\if :{?rows}
\else
\set rows 10000000
\endif
\if :{?work}
\set scratch :work
\endif
\ir ../../../../../fixtures/neutral/fieldobs.sql
SET debug_print_plan = on;
SET max_parallel_workers_per_gather = 0;
SET work_mem = '1GB';
SET effective_cache_size = '24GB';
SET jit = off;

-- Plain folds over the columnar table (decompose's PlainAgg breaker; answer.stats / answer.counted from part statistics)
-- 001 a whole-table count
EXPLAIN (COSTS OFF) SELECT count(*) FROM sightings;
-- 002 a count under a mostly-zero int2 filter (answer.counted)
EXPLAIN (COSTS OFF) SELECT count(*) FROM sightings WHERE flag_reason <> 0;
-- 003 sum / avg / max of small integers and a count in one fold
EXPLAIN (COSTS OFF) SELECT sum(individual_count), avg(duration_min), max(photo_height), count(*) FROM sightings;
-- 004 avg of a 63-bit int8 (numeric transition) and the sum of a nullable int8
EXPLAIN (COSTS OFF) SELECT avg(observer_id), sum(media_bytes) FROM sightings;
-- 005 min / max of a date and a timestamp
EXPLAIN (COSTS OFF) SELECT min(observed_on), max(observed_on), max(observed_at) FROM sightings;
-- 006 sums of one column shifted by constants (finals.cse / finals.shift: sum(x + k) from sum(x) and the count)
EXPLAIN (COSTS OFF) SELECT sum(elevation_m), sum(elevation_m + 10), sum(elevation_m + 100), sum(elevation_m - 5), sum(duration_min), sum(duration_min + 1), sum(duration_min + 60), sum(photo_width + 2) FROM sightings;
-- 007 a count under a substring LIKE on a URL column (dict.open over the code dictionary)
EXPLAIN (COSTS OFF) SELECT count(*) FROM sightings WHERE source_url LIKE '%atlas%';
-- 008 numeric, real and float8 aggregates under an int2 range filter
EXPLAIN (COSTS OFF) SELECT avg(temperature_c), sum(fee_paid), avg(accuracy_m), max(distance_km) FROM sightings WHERE quality_grade >= 2;

-- Distinct counts (keyed: distinct.identity over a fixed-width key, the code fronts over a text key)
-- 009 a high-cardinality int8
EXPLAIN (COSTS OFF) SELECT count(DISTINCT observer_id) FROM sightings;
-- 010 a phrase column that is mostly '' (COLLATE "C")
EXPLAIN (COSTS OFF) SELECT count(DISTINCT notes) FROM sightings;
-- 011 a derived text key: the host of a URL by regexp_replace with a capture group
EXPLAIN (COSTS OFF) SELECT count(DISTINCT regexp_replace(source_url, '^[a-z]+://(?:www\.)?([^/?]+).*$', '\1')) FROM sightings;
-- 012 a nullable text column under the database default collation (observers, columnar)
EXPLAIN (COSTS OFF) SELECT count(DISTINCT profile_url) FROM observers;
-- 013 a signed int4
EXPLAIN (COSTS OFF) SELECT count(DISTINCT cell_code) FROM sightings;

-- Flat filtered scans with LIMIT (pgrc scan: masks, late decode, hoisted quals)
-- 014 int8 / date / int2 columns under a selective int2 filter
EXPLAIN (COSTS OFF) SELECT checklist_id, observed_on, flag_reason FROM sightings WHERE flag_reason > 5 LIMIT 15;
-- 015 a text column under text <> ''
EXPLAIN (COSTS OFF) SELECT notes, observed_at FROM sightings WHERE notes <> '' LIMIT 10;
-- 016 the sorted prefix: equality on the first key, a range on the second, two text columns out
EXPLAIN (COSTS OFF) SELECT media_url, taxon_label FROM sightings WHERE site_id = 3 AND observed_on BETWEEN '2024-04-01' AND '2024-04-30' LIMIT 5;
-- 017 an int2 range and IS NOT NULL on a nullable text column, LIMIT with OFFSET
EXPLAIN (COSTS OFF) SELECT observer_id, elevation_m FROM sightings WHERE elevation_m > 2000 AND weather IS NOT NULL LIMIT 20 OFFSET 5;
-- 018 a signed int4 inequality and a timestamp range
EXPLAIN (COSTS OFF) SELECT observed_at, cell_code FROM sightings WHERE cell_code < 0 AND observed_at >= '2024-05-01 12:00:00' LIMIT 10;
-- 019 an existence probe: a constant target list under a case-sensitive LIKE
EXPLAIN (COSTS OFF) SELECT 1 FROM sightings WHERE taxon_label LIKE '%Heron%' LIMIT 1;
-- 020 every column of one row
EXPLAIN (COSTS OFF) SELECT * FROM sightings LIMIT 1;
-- 021 a point lookup on the unique int8
EXPLAIN (COSTS OFF) SELECT sighting_id, upload_key FROM sightings WHERE sighting_id = 123457;
-- 022 a point lookup on a large hashed int8
EXPLAIN (COSTS OFF) SELECT observer_id, observed_at FROM sightings WHERE observer_id = 1730890389612118042;
-- 023 int2 inequality, text <> '' and a date equality together
EXPLAIN (COSTS OFF) SELECT region_id, observer_id FROM sightings WHERE region_id <> 7 AND device_model <> '' AND observed_on = '2024-04-12' LIMIT 3;

-- Grouped aggregates ordered by an aggregate (runs: owner.count, the light / sparse code fronts, keyed distinct per group)
-- 024 an int2 key under its own filter, every group ordered by count (no LIMIT: a full sort of the groups)
EXPLAIN (COSTS OFF) SELECT flag_reason, count(*) FROM sightings WHERE flag_reason <> 0 GROUP BY flag_reason ORDER BY count(*) DESC;
-- 025 count(DISTINCT int8) per int2 group, top 10
EXPLAIN (COSTS OFF) SELECT region_id, count(DISTINCT observer_id) AS observers FROM sightings GROUP BY region_id ORDER BY observers DESC LIMIT 10;
-- 026 several aggregates and a distinct count per int2 group, top 10 by count
EXPLAIN (COSTS OFF) SELECT region_id, sum(individual_count), count(*) AS n, avg(duration_min), count(DISTINCT taxon_id) FROM sightings GROUP BY region_id ORDER BY n DESC LIMIT 10;
-- 027 a mostly-'' low-cardinality text key with a distinct count
EXPLAIN (COSTS OFF) SELECT device_model, count(DISTINCT observer_id) AS observers FROM sightings WHERE device_model <> '' GROUP BY device_model ORDER BY observers DESC LIMIT 10;
-- 028 an int2 and a text key with a distinct count
EXPLAIN (COSTS OFF) SELECT quality_grade, device_model, count(DISTINCT observer_id) AS observers FROM sightings WHERE device_model <> '' GROUP BY quality_grade, device_model ORDER BY observers DESC LIMIT 10;
-- 029 a phrase key, top 10 by count
EXPLAIN (COSTS OFF) SELECT notes, count(*) AS n FROM sightings WHERE notes <> '' GROUP BY notes ORDER BY n DESC LIMIT 10;
-- 030 a phrase key, top 10 by a distinct count
EXPLAIN (COSTS OFF) SELECT notes, count(DISTINCT observer_id) AS observers FROM sightings WHERE notes <> '' GROUP BY notes ORDER BY observers DESC LIMIT 10;
-- 031 a low-cardinality text key and a phrase key
EXPLAIN (COSTS OFF) SELECT method, notes, count(*) AS n FROM sightings WHERE notes <> '' GROUP BY method, notes ORDER BY n DESC LIMIT 10;
-- 032 a high-cardinality int8 key
EXPLAIN (COSTS OFF) SELECT observer_id, count(*) FROM sightings GROUP BY observer_id ORDER BY count(*) DESC LIMIT 10;
-- 033 an int8 and a text key (owner.count over the group record's count)
EXPLAIN (COSTS OFF) SELECT observer_id, notes, count(*) FROM sightings GROUP BY observer_id, notes ORDER BY count(*) DESC LIMIT 10;
-- 034 the same keys with no ORDER BY: any ten groups (topk.walk: the group walk on the first key)
EXPLAIN (COSTS OFF) SELECT observer_id, notes, count(*) FROM sightings GROUP BY observer_id, notes LIMIT 10;
-- 035 three keys, one an extract() of a timestamp
EXPLAIN (COSTS OFF) SELECT observer_id, extract(hour FROM observed_at) AS h, notes, count(*) FROM sightings GROUP BY observer_id, h, notes ORDER BY count(*) DESC LIMIT 10;
-- 036 a substring LIKE filter, a phrase key and min() of a text column
EXPLAIN (COSTS OFF) SELECT notes, min(source_url), count(*) AS n FROM sightings WHERE source_url LIKE '%atlas%' AND notes <> '' GROUP BY notes ORDER BY n DESC LIMIT 10;
-- 037 LIKE and NOT LIKE together, two text min()s and a distinct count per group
EXPLAIN (COSTS OFF) SELECT notes, min(media_url), min(taxon_label), count(*) AS n, count(DISTINCT observer_id) FROM sightings WHERE taxon_label LIKE '%Owl%' AND media_url NOT LIKE '%.fieldobs.%' AND notes <> '' GROUP BY notes ORDER BY n DESC LIMIT 10;

-- Row top-k (runs.topk_rows: the row walk on a timestamp, late fetch of a wide row, text sort keys and their ties)
-- 038 every column of the first ten rows by time under a LIKE (topk.fetch)
EXPLAIN (COSTS OFF) SELECT * FROM sightings WHERE source_url LIKE '%atlas%' ORDER BY observed_at LIMIT 10;
-- 039 one text column of the first ten rows by time
EXPLAIN (COSTS OFF) SELECT notes FROM sightings WHERE notes <> '' ORDER BY observed_at LIMIT 10;
-- 040 a text sort key (code.cut / code.heads / code.ties)
EXPLAIN (COSTS OFF) SELECT notes FROM sightings WHERE notes <> '' ORDER BY notes LIMIT 10;
-- 041 a timestamp then a text sort key
EXPLAIN (COSTS OFF) SELECT notes FROM sightings WHERE notes <> '' ORDER BY observed_at, notes LIMIT 10;
-- 042 the last ten rows by time (DESC)
EXPLAIN (COSTS OFF) SELECT taxon_label, observed_at FROM sightings WHERE has_media ORDER BY observed_at DESC LIMIT 10;

-- HAVING, expression keys and wide keys
-- 043 avg(octet_length) of a URL per int4 key with HAVING, ordered by the average
EXPLAIN (COSTS OFF) SELECT site_id, avg(octet_length(media_url)) AS len, count(*) AS n FROM sightings WHERE media_url <> '' GROUP BY site_id HAVING count(*) > 1000 ORDER BY len DESC LIMIT 25;
-- 044 a regexp_replace() key (literal arguments placed as words), min() of the column, HAVING
EXPLAIN (COSTS OFF) SELECT regexp_replace(source_url, '^[a-z]+://(?:www\.)?([^/?]+).*$', '\1') AS host, avg(octet_length(source_url)) AS len, count(*) AS n, min(source_url) FROM sightings WHERE source_url <> '' GROUP BY host HAVING count(*) > 1000 ORDER BY len DESC LIMIT 25;
-- 045 an int8 and a signed int4 key with sums, under a text filter (code.sparse)
EXPLAIN (COSTS OFF) SELECT checklist_id, cell_code, count(*) AS n, sum(individual_count), avg(photo_width) FROM sightings WHERE notes <> '' GROUP BY checklist_id, cell_code ORDER BY n DESC LIMIT 10;
-- 046 an int2 and a signed int4 key with a boolean sum
EXPLAIN (COSTS OFF) SELECT quality_grade, cell_code, count(*) AS n, sum(is_verified::int), avg(duration_min) FROM sightings WHERE notes <> '' GROUP BY quality_grade, cell_code ORDER BY n DESC LIMIT 10;
-- 047 a unique int8 and an int4 key, no filter (every group of one row)
EXPLAIN (COSTS OFF) SELECT sighting_id, cell_code, count(*) AS n FROM sightings GROUP BY sighting_id, cell_code ORDER BY n DESC LIMIT 10;
-- 048 a URL key
EXPLAIN (COSTS OFF) SELECT media_url, count(*) AS n FROM sightings GROUP BY media_url ORDER BY n DESC LIMIT 10;
-- 049 a constant group key beside a URL key
EXPLAIN (COSTS OFF) SELECT 'all' AS bucket, source_url, count(*) AS n FROM sightings GROUP BY 1, source_url ORDER BY n DESC LIMIT 10;
-- 050 an int4 key with keys derived from it by arithmetic
EXPLAIN (COSTS OFF) SELECT cell_code, cell_code / 1024 AS block, cell_code % 7 AS lane, count(*) AS n FROM sightings GROUP BY cell_code, cell_code / 1024, cell_code % 7 ORDER BY n DESC LIMIT 10;
-- 051 a concatenated text key (url || int::text: not per-code evaluable, keyed by the varlena arm)
EXPLAIN (COSTS OFF) SELECT media_url || site_id::text AS k, count(*) FROM sightings GROUP BY 1 ORDER BY 2 DESC LIMIT 10;

-- The sorted prefix under group keys (verdicts over the leading sort keys: zone, psma, bloom, code range, elision)
-- 052 a URL key under site, date range, int2 and boolean equalities
EXPLAIN (COSTS OFF) SELECT media_url, count(*) AS n FROM sightings WHERE site_id = 2 AND observed_on BETWEEN '2024-04-01' AND '2024-04-30' AND flag_reason = 0 AND NOT is_verified AND media_url <> '' GROUP BY media_url ORDER BY n DESC LIMIT 10;
-- 053 a title-like key under the same prefix
EXPLAIN (COSTS OFF) SELECT taxon_label, count(*) AS n FROM sightings WHERE site_id = 2 AND observed_on BETWEEN '2024-04-01' AND '2024-04-30' AND quality_grade = 0 AND taxon_label <> '' GROUP BY taxon_label ORDER BY n DESC LIMIT 10;
-- 054 a URL key with LIMIT 10 OFFSET 1000
EXPLAIN (COSTS OFF) SELECT source_url, count(*) AS n FROM sightings WHERE site_id = 1 AND observed_on BETWEEN '2024-03-01' AND '2024-05-31' AND is_verified AND photo_width <> 0 AND flag_reason = 0 GROUP BY source_url ORDER BY n DESC LIMIT 10 OFFSET 1000;
-- 055 a CASE key choosing a text column (key.codeif) among int2 / text keys, OFFSET 1000
EXPLAIN (COSTS OFF) SELECT method, quality_grade, flag_reason, CASE WHEN quality_grade = 0 AND flag_reason = 0 THEN source_url ELSE '' END AS origin, media_url AS media, count(*) AS n FROM sightings WHERE site_id = 1 AND observed_on BETWEEN '2024-03-01' AND '2024-05-31' AND has_media GROUP BY method, quality_grade, flag_reason, origin, media ORDER BY n DESC LIMIT 10 OFFSET 1000;
-- 056 an int8 and a date key under an IN list and an int8 equality, OFFSET 100
EXPLAIN (COSTS OFF) SELECT checklist_id, observed_on, count(*) AS n FROM sightings WHERE site_id = 1 AND observed_on BETWEEN '2024-03-01' AND '2024-05-31' AND quality_grade IN (0, 3) AND observer_id = 1730890389612118042 GROUP BY checklist_id, observed_on ORDER BY n DESC LIMIT 10 OFFSET 100;
-- 057 two int2 keys under an int4 equality, OFFSET 10000
EXPLAIN (COSTS OFF) SELECT photo_width, photo_height, count(*) AS n FROM sightings WHERE site_id = 1 AND observed_on BETWEEN '2024-03-01' AND '2024-05-31' AND flag_reason = 0 AND has_media AND taxon_id = 7 GROUP BY photo_width, photo_height ORDER BY n DESC LIMIT 10 OFFSET 10000;
-- 058 a date_trunc('minute') key ordered by the key, OFFSET 100
EXPLAIN (COSTS OFF) SELECT date_trunc('minute', observed_at) AS minute, count(*) AS n FROM sightings WHERE site_id = 1 AND observed_on BETWEEN '2024-04-10' AND '2024-04-11' AND flag_reason = 0 GROUP BY date_trunc('minute', observed_at) ORDER BY date_trunc('minute', observed_at) LIMIT 10 OFFSET 100;

-- Analytical training corpus over the generated "pulse" fixture (corpus/analytics-pulse-fixture.sql:
-- events, events_heap, app_users, tenants, event_kinds, app_releases). One statement per line.
-- Written from the operator-class list in pgo/README.md ("Shape classes covered"), not from any
-- published query list, and never extended to move a published benchmark's per-query numbers (README,
-- "Adding to the corpus"). The lint (pgo/lint-training-overlap.sh) checks it mechanically twice over:
-- no statement is a published one (exact canonical form) and none is a published one with its columns
-- renamed (column-blind skeleton). The per-line engine assignment lives in corpus/analytics-pulse-engines.tsv.
--
-- J1 sorted-key range prune + count (a three-day window of the leading sort key)
SELECT count(*) FROM events WHERE occurred_on BETWEEN DATE '2025-04-07' AND DATE '2025-04-09';
-- J2 null-aware counting battery (FILTER clauses and count(col) over nullable columns)
SELECT count(*) FILTER (WHERE user_id IS NULL) AS anonymous, count(referrer), count(search_query), count(payload), count(stack_trace), count(*) FILTER (WHERE consent IS NULL) FROM events;
-- J3 scalar aggregate battery across int2 / int4 / int8 / float8 / real / numeric
SELECT sum(bytes_out), avg(duration_ms), min(latency_ms), max(latency_ms), avg(cpu_pct), sum(amount), avg(amount), max(retry_count) FROM events;
-- J4 statistical and boolean aggregates over the last month
SELECT stddev_samp(latency_ms), var_pop(duration_ms), corr(duration_ms, latency_ms), bool_and(consent), bool_or(is_error), min(occurred_at), max(ingested_at) FROM events WHERE occurred_on >= DATE '2025-05-01';
-- J5 min / max over text: a dictionary-coded column and a random one, under a text equality
SELECT min(app_version), max(app_version), min(request_id), max(request_id), min(page_path) FROM events WHERE platform = 'web';
-- J6 global distinct over a wide integer under a text IN-list
SELECT count(DISTINCT user_id) FROM events WHERE platform IN ('ios', 'android');
-- J7 several global distinct counts in one pass over dictionary-coded text (the every-lane entries window; page_path and session_key are large dictionaries)
SELECT count(DISTINCT page_path), count(DISTINCT session_key), count(DISTINCT app_version), count(DISTINCT country_code) FROM events;
-- J8 distinct over a large-dictionary text column, predicate on the same column
SELECT count(DISTINCT page_path) FROM events WHERE page_path LIKE '/files/%';
-- J9 global distinct over random high-cardinality text and a wide integer within a week
SELECT count(DISTINCT request_id), count(DISTINCT client_ip) FROM events WHERE occurred_on BETWEEN DATE '2025-05-05' AND DATE '2025-05-11';
-- J10 per-group distinct, low-cardinality text key, unbounded result
SELECT country_code, count(DISTINCT user_id) AS users, count(*) AS events FROM events GROUP BY country_code ORDER BY users DESC, country_code;
-- J11 per-group distinct over text and integer, keyed by the sorted date column
SELECT occurred_on, count(DISTINCT session_key) AS sessions, count(DISTINCT user_id) AS users FROM events WHERE occurred_on >= DATE '2025-05-15' GROUP BY occurred_on ORDER BY occurred_on;
-- J12 per-group distinct with HAVING on the distinct count, medium-cardinality key
SELECT tenant_id, count(DISTINCT user_id) AS active_users FROM events WHERE user_id IS NOT NULL GROUP BY tenant_id HAVING count(DISTINCT user_id) >= 50 ORDER BY active_users DESC, tenant_id LIMIT 25;
-- J13 low-cardinality smallint key, unbounded, ordered by the key
SELECT status_code, count(*), avg(duration_ms), max(bytes_out) FROM events GROUP BY status_code ORDER BY status_code;
-- J14 composite low-cardinality text keys
SELECT platform, http_method, count(*) AS n, sum(bytes_out) AS bytes FROM events GROUP BY platform, http_method ORDER BY platform, n DESC;
-- J15 medium-cardinality integer key, top-N by a sum
SELECT tenant_id, count(*) AS n, sum(duration_ms) AS total_ms, max(occurred_at) AS last_seen FROM events GROUP BY tenant_id ORDER BY total_ms DESC LIMIT 40;
-- J16 very-high-cardinality wide-integer key with min / max timestamps
SELECT user_id, count(*) AS n, min(occurred_at), max(occurred_at) FROM events WHERE user_id IS NOT NULL GROUP BY user_id ORDER BY n DESC, user_id LIMIT 30;
-- J17 very-high-cardinality text key (session) with HAVING
SELECT session_key, count(*) AS n, sum(duration_ms) FROM events GROUP BY session_key HAVING count(*) > 9 ORDER BY n DESC, session_key LIMIT 30;
-- J18 unique random text key over a bounded window (duplicate detection, empty result)
SELECT request_id, count(*) FROM events WHERE occurred_on BETWEEN DATE '2025-03-20' AND DATE '2025-03-22' GROUP BY request_id HAVING count(*) > 1;
-- J19 large-dictionary text key under a prefix filter, with a per-group distinct
SELECT page_path, count(*) AS n, count(DISTINCT user_id) AS users FROM events WHERE page_path LIKE '/projects/%' GROUP BY page_path ORDER BY n DESC, page_path LIMIT 20;
-- J20 extract()-derived composite key (day of week x hour)
SELECT extract(dow FROM occurred_at) AS dow, extract(hour FROM occurred_at) AS hr, count(*), avg(latency_ms) FROM events GROUP BY 1, 2 ORDER BY 1, 2;
-- J21 date_trunc key over a timestamp with numeric revenue sums
SELECT date_trunc('week', occurred_at) AS wk, sum(amount) AS revenue, count(amount) AS purchases FROM events GROUP BY wk ORDER BY wk;
-- J22 CASE-bucketed key crossed with a text key
SELECT CASE WHEN duration_ms < 100 THEN 'fast' WHEN duration_ms < 1000 THEN 'ok' WHEN duration_ms < 5000 THEN 'slow' ELSE 'stalled' END AS speed, platform, count(*), avg(latency_ms) FROM events GROUP BY 1, 2 ORDER BY 2, 1;
-- J23 split_part()-derived key from a path, with a per-group distinct
SELECT split_part(page_path, '/', 2) AS section, count(*) AS n, count(DISTINCT session_key) AS sessions FROM events GROUP BY 1 ORDER BY n DESC;
-- J24 arithmetic binning keys over int8 and int4
SELECT bytes_out / 1048576 AS mib, (duration_ms / 250) * 250 AS ms_bin, count(*) FROM events WHERE bytes_out < 52428800 GROUP BY 1, 2 ORDER BY 3 DESC, 1, 2 LIMIT 50;
-- J25 HAVING over a derived rate, composite text keys
SELECT app_version, platform, count(*) AS n, avg(CASE WHEN is_error THEN 1.0 ELSE 0.0 END) AS err_rate FROM events GROUP BY app_version, platform HAVING count(*) > 1000 AND avg(CASE WHEN is_error THEN 1.0 ELSE 0.0 END) > 0.004 ORDER BY err_rate DESC, app_version, platform;
-- J26 nullable smallint and nullable boolean keys (NULL groups)
SELECT experiment_bucket, consent, count(*), avg(duration_ms) FROM events GROUP BY experiment_bucket, consent ORDER BY experiment_bucket NULLS FIRST, consent NULLS FIRST;
-- J27 ordered-set aggregates per group over one day
SELECT platform, percentile_cont(0.5) WITHIN GROUP (ORDER BY latency_ms) AS p50, percentile_disc(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95, mode() WITHIN GROUP (ORDER BY country_code) AS top_country FROM events WHERE occurred_on = DATE '2025-04-02' GROUP BY platform ORDER BY platform;
-- J28 timestamptz date_trunc key (session time zone) over one day
SELECT date_trunc('hour', ingested_at) AS h, count(*), max(latency_ms) FROM events WHERE occurred_on = DATE '2025-03-15' GROUP BY 1 ORDER BY 1;
-- J29 string_agg / array_agg with DISTINCT and ORDER BY inside the aggregate
SELECT tenant_id, string_agg(DISTINCT platform, ',' ORDER BY platform) AS platforms, array_agg(DISTINCT status_code ORDER BY status_code) AS codes, count(*) FROM events WHERE tenant_id <= 40 GROUP BY tenant_id ORDER BY tenant_id;
-- J30 pivot through FILTER clauses, ranked by a CASE sum
SELECT tenant_id, count(*) FILTER (WHERE platform = 'web') AS web, count(*) FILTER (WHERE platform = 'ios') AS ios, count(*) FILTER (WHERE platform = 'android') AS android, sum(CASE WHEN is_error THEN 1 ELSE 0 END) AS errors FROM events GROUP BY tenant_id ORDER BY errors DESC, tenant_id LIMIT 20;
-- J31 grouped aggregate over a grouped subquery (session-length histogram)
SELECT n_events, count(*) AS sessions FROM (SELECT session_key, count(*) AS n_events FROM events WHERE occurred_on BETWEEN DATE '2025-04-14' AND DATE '2025-04-20' GROUP BY session_key) s GROUP BY n_events ORDER BY n_events;
-- J32 integer top-N projecting narrow columns
SELECT event_id, tenant_id, duration_ms, bytes_out FROM events WHERE status_code = 200 ORDER BY bytes_out DESC, event_id LIMIT 25;
-- J33 timestamp top-N, latest first, under a boolean filter
SELECT occurred_at, tenant_id, page_path, status_code FROM events WHERE is_error ORDER BY occurred_at DESC LIMIT 50;
-- J34 sorted distinct over a NULL-heavy dictionary column with a pattern on the same column
SELECT DISTINCT search_query FROM events WHERE search_query LIKE 'cloud %' ORDER BY search_query;
-- J35 multi-key sort with mixed directions over one day
SELECT tenant_id, occurred_at, event_id, status_code FROM events WHERE occurred_on = DATE '2025-04-20' ORDER BY tenant_id, occurred_at DESC, event_id LIMIT 100;
-- J36 deep OFFSET under a two-key sort
SELECT event_id, occurred_at, duration_ms FROM events WHERE platform = 'android' ORDER BY duration_ms DESC, event_id OFFSET 20000 LIMIT 20;
-- J37 numeric top-N with NULLS LAST and a float tiebreak
SELECT request_id, amount, latency_ms FROM events WHERE amount IS NOT NULL ORDER BY amount DESC NULLS LAST, latency_ms LIMIT 30;
-- J38 two text sort keys (one long) over a twelve-hour window, larger LIMIT
SELECT country_code, user_agent, event_id FROM events WHERE occurred_at >= TIMESTAMP '2025-05-20 06:00:00' AND occurred_at < TIMESTAMP '2025-05-20 18:00:00' ORDER BY country_code, user_agent DESC, event_id LIMIT 200;
-- J39 full sort without LIMIT over a tenant / date slice
SELECT session_key, seq_in_session, page_path, occurred_at FROM events WHERE tenant_id = 7 AND occurred_on >= DATE '2025-05-20' ORDER BY session_key, seq_in_session, occurred_at;
-- J40 substring match and a negated prefix match, counted
SELECT count(*) FROM events WHERE message LIKE '%timeout%' AND referrer NOT LIKE 'https://www.%';
-- J41 case-insensitive pattern over a long string, grouped
SELECT platform, count(*) FROM events WHERE user_agent ILIKE '%android 1_;%' GROUP BY platform ORDER BY 2 DESC, 1;
-- J42 anchored regular expression with alternation and a bounded repeat
SELECT count(*), count(DISTINCT tenant_id) FROM events WHERE page_path ~ '^/(files|reports)/[a-z]+/[0-9]{6,}/';
-- J43 regexp_replace-derived template key over one day of log messages
SELECT regexp_replace(message, '[0-9]+', 'N', 'g') AS template, count(*) FROM events WHERE occurred_on = DATE '2025-03-03' GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 20;
-- J44 case-insensitive regexp over a NULL-heavy column with a NOT LIKE
SELECT http_method, count(*) FROM events WHERE search_query ~* '(report|export)' AND page_path NOT LIKE '%/view%' GROUP BY http_method ORDER BY http_method;
-- J45 string-length statistics per group, including a long sparse column
SELECT platform, min(length(user_agent)), max(length(user_agent)), avg(octet_length(message)), sum(length(payload)), max(length(stack_trace)) FROM events GROUP BY platform ORDER BY platform;
-- J46 strpos predicate, concatenated and to_char projections, top-N by id
SELECT event_id, upper(platform) || ':' || app_version AS tag, to_char(occurred_at, 'YYYY-MM-DD HH24:MI') AS ts FROM events WHERE retry_count >= 3 AND strpos(message, 'retries=') > 0 AND occurred_on = DATE '2025-04-04' ORDER BY event_id LIMIT 50;
-- J47 JSON-shaped sparse text: regexp substring key under a substring match
SELECT substring(payload from '"kind":([0-9]+)') AS kind, count(*) FROM events WHERE payload LIKE '%"retry":true%' GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10;
-- J48 OR-of-disjunctions predicate, counted
SELECT count(*) FROM events WHERE (status_code = 429 OR retry_count > 2) AND (platform = 'api' OR user_agent LIKE '%bot%');
-- J49 fact x columnar dimension hash join, grouped by a dimension attribute
SELECT t.plan, count(*) AS events, sum(e.bytes_out) AS bytes FROM events e JOIN tenants t ON t.tenant_id = e.tenant_id GROUP BY t.plan ORDER BY events DESC;
-- J50 fact x heap dimension join with a dimension-side filter
SELECT k.category, count(*), avg(e.duration_ms) FROM events e JOIN event_kinds k ON k.event_type_id = e.event_type_id WHERE k.is_billable GROUP BY k.category ORDER BY 2 DESC, 1;
-- J51 three-way join through a large build side (users), per-group distinct
SELECT u.locale, t.region, count(DISTINCT e.user_id) AS users FROM events e JOIN app_users u ON u.user_id = e.user_id JOIN tenants t ON t.tenant_id = u.tenant_id WHERE e.occurred_on >= DATE '2025-05-01' GROUP BY u.locale, t.region ORDER BY users DESC, 1, 2 LIMIT 30;
-- J52 text-key left join keeping unmatched probe rows (IS NULL anti pattern)
SELECT e.app_version, count(*) FROM events e LEFT JOIN app_releases r ON r.app_version = e.app_version WHERE r.app_version IS NULL GROUP BY e.app_version ORDER BY 2 DESC, 1;
-- J53 left join with NULL-keyed probe rows (anonymous events), grouped by a dimension column
SELECT u.role, count(*) AS events, count(u.user_id) AS known FROM events e LEFT JOIN app_users u ON u.user_id = e.user_id WHERE e.occurred_on = DATE '2025-05-27' GROUP BY u.role ORDER BY u.role NULLS FIRST;
-- J54 semi join through EXISTS (users who purchased)
SELECT count(*) FROM app_users u WHERE EXISTS (SELECT 1 FROM events e WHERE e.user_id = u.user_id AND e.amount IS NOT NULL);
-- J55 anti join through NOT EXISTS (users idle in the last two weeks), grouped
SELECT u.tenant_id, count(*) AS idle FROM app_users u WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.user_id = u.user_id AND e.occurred_on >= DATE '2025-05-16') GROUP BY u.tenant_id ORDER BY idle DESC, u.tenant_id LIMIT 20;
-- J56 IN (subquery) semi join against a filtered dimension
SELECT platform, count(*) FROM events WHERE tenant_id IN (SELECT tenant_id FROM tenants WHERE plan = 'enterprise' AND region LIKE 'eu-%') GROUP BY platform ORDER BY platform;
-- J57 NOT IN (subquery) over a non-null heap key
SELECT count(*) FROM events WHERE event_type_id NOT IN (SELECT event_type_id FROM event_kinds WHERE category = 'system');
-- J58 self-join on a text key plus an integer offset (consecutive steps in a session) over one day
SELECT a.page_path AS from_page, b.page_path AS to_page, count(*) AS n FROM events a JOIN events b ON b.session_key = a.session_key AND b.seq_in_session = a.seq_in_session + 1 WHERE a.occurred_on = DATE '2025-04-11' AND b.occurred_on = DATE '2025-04-11' GROUP BY 1, 2 ORDER BY n DESC, 1, 2 LIMIT 20;
-- J59 correlated scalar subquery in WHERE (an aggregate over the correlated key; decorrelates to a grouped join)
SELECT t.tenant_id, t.plan, t.mrr FROM tenants t WHERE t.region = 'sa-east' AND t.seats > 2000 AND t.mrr > (SELECT 50 * avg(e.amount) FROM events e WHERE e.tenant_id = t.tenant_id) ORDER BY t.tenant_id;
-- J60 correlated scalar subquery in the select list over a small outer relation
SELECT k.event_name, (SELECT count(*) FROM events e WHERE e.event_type_id = k.event_type_id AND e.occurred_on = DATE '2025-05-02') AS n FROM event_kinds k WHERE k.category = 'billing' ORDER BY n DESC, k.event_name;
-- J61 uncorrelated scalar subquery (a percentile) feeding a filter
SELECT count(*), avg(duration_ms) FROM events WHERE latency_ms > (SELECT percentile_cont(0.99) WITHIN GROUP (ORDER BY latency_ms) FROM events WHERE occurred_on = DATE '2025-03-31');
-- J62 mixed-storage join: heap slice x columnar users, grouped
SELECT h.tenant_id, count(*) AS n FROM events_heap h JOIN app_users u ON u.user_id = h.user_id WHERE u.role = 'admin' GROUP BY h.tenant_id ORDER BY n DESC, h.tenant_id LIMIT 10;
-- J63 rank() over partitions of a grouped result (top users per tenant)
SELECT tenant_id, user_id, n, r FROM (SELECT tenant_id, user_id, count(*) AS n, rank() OVER (PARTITION BY tenant_id ORDER BY count(*) DESC) AS r FROM events WHERE occurred_on BETWEEN DATE '2025-04-01' AND DATE '2025-04-03' AND user_id IS NOT NULL GROUP BY tenant_id, user_id) s WHERE r <= 3 ORDER BY tenant_id, r, user_id LIMIT 300;
-- J64 row_number() step numbering inside sessions
SELECT session_key, page_path, occurred_at, row_number() OVER (PARTITION BY session_key ORDER BY occurred_at, event_id) AS step FROM events WHERE tenant_id = 3 AND occurred_on = DATE '2025-05-05' ORDER BY session_key, step;
-- J65 running and moving aggregates over a grouped daily series
SELECT occurred_on, sum(amount) AS revenue, sum(sum(amount)) OVER (ORDER BY occurred_on) AS cumulative, avg(sum(amount)) OVER (ORDER BY occurred_on ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) AS avg7 FROM events WHERE amount IS NOT NULL GROUP BY occurred_on ORDER BY occurred_on;
-- J66 lag() gaps, dense_rank() and ntile() over a user slice, with a named window
SELECT user_id, event_id, occurred_at - lag(occurred_at) OVER w AS gap, dense_rank() OVER (PARTITION BY user_id ORDER BY event_type_id) AS kind_rank, ntile(4) OVER w AS quartile FROM events WHERE user_id BETWEEN 1000 AND 1040 WINDOW w AS (PARTITION BY user_id ORDER BY occurred_at, event_id) ORDER BY user_id, occurred_at, event_id;
-- J67 framed window aggregates (ROWS frame, first_value, max) over a per-tenant daily series
SELECT tenant_id, occurred_on, n, max(n) OVER w AS peak7, first_value(n) OVER w AS first7, n - coalesce(lag(n) OVER (PARTITION BY tenant_id ORDER BY occurred_on), 0) AS delta FROM (SELECT tenant_id, occurred_on, count(*) AS n FROM events WHERE tenant_id <= 20 GROUP BY tenant_id, occurred_on) d WINDOW w AS (PARTITION BY tenant_id ORDER BY occurred_on ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) ORDER BY tenant_id, occurred_on;
-- J68 CTE feeding an outer aggregate (active tenants per day)
WITH daily AS (SELECT occurred_on, tenant_id, count(*) AS n FROM events GROUP BY occurred_on, tenant_id) SELECT occurred_on, count(*) AS active_tenants, max(n), sum(n) FROM daily WHERE n >= 100 GROUP BY occurred_on ORDER BY occurred_on;
-- J69 MATERIALIZED CTE read by three scalar subqueries
WITH heavy AS MATERIALIZED (SELECT user_id, count(*) AS n FROM events WHERE user_id IS NOT NULL GROUP BY user_id HAVING count(*) > 40) SELECT (SELECT count(*) FROM heavy) AS users, (SELECT avg(n) FROM heavy) AS mean, (SELECT max(n) FROM heavy) AS peak;
-- J70 UNION ALL of two filtered aggregates with constant tags
SELECT 'error' AS kind, platform, count(*) FROM events WHERE is_error GROUP BY platform UNION ALL SELECT 'bot', platform, count(*) FROM events WHERE is_bot GROUP BY platform ORDER BY 1, 2;
-- J71 UNION (distinct) across a fact column and a dimension column
SELECT country_code FROM events WHERE occurred_on = DATE '2025-03-08' UNION SELECT country_code FROM app_users ORDER BY 1;
-- J72 INTERSECT of two weekly user sets (retention), counted
SELECT count(*) FROM (SELECT user_id FROM events WHERE occurred_on BETWEEN DATE '2025-03-01' AND DATE '2025-03-07' INTERSECT SELECT user_id FROM events WHERE occurred_on BETWEEN DATE '2025-05-23' AND DATE '2025-05-29') s;
-- J73 EXCEPT: tenants with no activity in May, counted
SELECT count(*) FROM (SELECT tenant_id FROM tenants EXCEPT SELECT tenant_id FROM events WHERE occurred_on >= DATE '2025-05-01') s;
-- J74 recursive CTE over the tenant hierarchy
WITH RECURSIVE tree AS (SELECT tenant_id, parent_tenant_id, 0 AS depth FROM tenants WHERE parent_tenant_id IS NULL AND tenant_id <= 500 UNION ALL SELECT c.tenant_id, c.parent_tenant_id, p.depth + 1 FROM tenants c JOIN tree p ON c.parent_tenant_id = p.tenant_id WHERE p.depth < 4) SELECT depth, count(*) FROM tree GROUP BY depth ORDER BY depth;
-- J75 ROLLUP over two text keys within a week
SELECT platform, country_code, count(*), sum(bytes_out) FROM events WHERE occurred_on >= DATE '2025-05-22' GROUP BY ROLLUP (platform, country_code) ORDER BY platform NULLS LAST, country_code NULLS LAST;
-- J76 GROUPING SETS with grouping() bits
SELECT http_method, status_code, grouping(http_method, status_code) AS g, count(*) FROM events GROUP BY GROUPING SETS ((http_method), (status_code), ()) ORDER BY g, http_method, status_code;
-- J77 CUBE over a joined dimension attribute and a boolean (JOIN ... USING)
SELECT t.plan, e.is_error, count(*) FROM events e JOIN tenants t USING (tenant_id) WHERE e.occurred_on = DATE '2025-04-30' GROUP BY CUBE (t.plan, e.is_error) ORDER BY 1, 2;
-- J78 DISTINCT ON: the latest event per tenant on one day
SELECT DISTINCT ON (tenant_id) tenant_id, occurred_at, page_path, status_code FROM events WHERE occurred_on = DATE '2025-05-10' ORDER BY tenant_id, occurred_at DESC, event_id;
-- J79 full-width projection under a range predicate on random text
SELECT * FROM events WHERE request_id >= 'abc1' AND request_id < 'abc2' ORDER BY event_id;
-- J80 full-width projection of a sorted-key slice with a top-N on time
SELECT * FROM events WHERE occurred_on = DATE '2025-03-09' AND tenant_id = 12 ORDER BY occurred_at, event_id LIMIT 100;
-- J81 wide join projection (every fact column plus dimension columns) over an id range
SELECT e.*, t.plan, t.region, k.event_name FROM events e JOIN tenants t ON t.tenant_id = e.tenant_id JOIN event_kinds k ON k.event_type_id = e.event_type_id WHERE e.event_id BETWEEN 4000000 AND 4000250 ORDER BY e.event_id;
-- J82 heap point lookup through the unique index, full row
SELECT * FROM events_heap WHERE event_id = 1234567;
-- J83 heap IN-list of point lookups
SELECT event_id, user_id, page_path, status_code FROM events_heap WHERE event_id IN (17, 4099, 77777, 123456, 654321, 1000001, 1500000) ORDER BY event_id;
-- J84 heap composite-index range scan, backward, top-N
SELECT occurred_at, page_path, status_code FROM events_heap WHERE user_id = 4242 ORDER BY occurred_at DESC LIMIT 20;
-- J85 heap text-index point lookup, the key computed by an immutable function
SELECT event_id, tenant_id, occurred_at, duration_ms FROM events_heap WHERE request_id = md5('req' || '31337');
-- J86 heap sequential scan feeding a grouped aggregate
SELECT platform, count(*), avg(latency_ms), max(occurred_at) FROM events_heap WHERE status_code >= 400 GROUP BY platform ORDER BY platform;
-- J87 index nested loop from a columnar outer into the heap's composite index
SELECT u.email, h.occurred_at, h.page_path FROM app_users u JOIN events_heap h ON h.user_id = u.user_id WHERE u.user_id BETWEEN 200 AND 210 ORDER BY u.user_id, h.occurred_at LIMIT 200;
-- J88 interval arithmetic across timestamptz and timestamp, extract(epoch) of an interval
SELECT platform, avg(extract(epoch FROM ingested_at - occurred_at)) AS lag_s, max(ingested_at - occurred_at) FROM events WHERE occurred_on = DATE '2025-04-18' GROUP BY platform ORDER BY platform;

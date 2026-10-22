-- Fixture for the analytical training corpus (corpus/analytics-pulse.sql): "pulse", the event log of a
-- multi-tenant SaaS product plus four dimensions. It is generated here, in SQL, from generate_series and
-- integer hashing (hashint8extended with fixed seeds, md5 of the row number, fixed word arrays): no
-- random(), no clock, no external data, so every training host builds the same rows. None of its
-- tables or columns come from a published benchmark's schema.
--
-- Run the copy gen-corpus.sh emits (analytics-fixture.sql): it prepends the scale and the two server-side
-- scratch files (\set n_events / n_users / n_tenants / n_heap / events_tsv / users_tsv). The server writes
-- and reads those files (COPY ... TO / FROM a file: superuser, same host); the caller deletes them after.
--
--   relation      AM            rows (default)   role
--   events        pgrcolumnar  n_events (8M)    the wide fact table: 36 columns, NOT NULL where the data
--                                                has no NULLs, COLLATE "C" text, sorted_by (day, tenant, time);
--                                                loaded by COPY TO + COPY FROM a text file (the parallel path)
--   app_users     pgrcolumnar  n_events / 5     users; email is unique (millions of distinct values); COPY
--   tenants       pgrcolumnar  n_tenants (20k)  tenants; CREATE TABLE AS (the CTAS receiver; nullable columns)
--   event_kinds   heap + PK     400              event-type dimension
--   app_releases  heap + PK     205              release dimension, keyed by a text version (35 missing)
--   events_heap   heap + 3 idx  n_heap (1.6M)    the first n_heap event ids of events: heap scans, btree
--                                                point and range lookups, index nested loops
--
-- Data shapes (see pgo/README.md "The fixture"): uniform and product-skewed integer keys (tenant, user,
-- event kind, version, country); a sorted timestamp/date pair; low-cardinality dictionary text (platform,
-- method, country, version, user agent); large-dictionary text (page_path: ~55% of rows from 1,200 static
-- routes, the rest entity routes whose ids recur within a few days; session_key: ~6 rows per value);
-- unique random text (request_id = md5, message); long and sparse text (user_agent 45-115 bytes, payload
-- ~95 bytes on 25% of rows, stack_trace 0.3-11 KB on 0.6%); NULLs in eight columns; int2 / int4 / int8 /
-- numeric(12,2) / float8 / real / boolean / date / timestamp / timestamptz.
\set ON_ERROR_STOP 1
\if :{?n_events}
\else
DO $$ BEGIN RAISE EXCEPTION 'analytics fixture: run the copy gen-corpus.sh emits (it sets n_events, n_users, n_tenants, n_heap, events_tsv, users_tsv)'; END $$;
\endif
SET client_min_messages = warning;
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar') THEN CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
DROP TABLE IF EXISTS events_heap, events, app_users, tenants, event_kinds, app_releases;

\echo analytics fixture: dimensions
CREATE TABLE event_kinds (
    event_type_id smallint PRIMARY KEY,
    event_name text COLLATE "C" NOT NULL,
    category text COLLATE "C" NOT NULL,
    is_billable boolean NOT NULL,
    weight numeric(6,3) NOT NULL,
    introduced_on date NOT NULL
);
INSERT INTO event_kinds
SELECT s.k::int2,
       s.cat || '.' || s.verb || '.' || s.k::text,
       s.cat,
       s.cat = 'billing' OR s.k % 23 = 0,
       ((s.k * 37) % 1000) / 100.0 + 0.5,
       DATE '2023-01-01' + s.k * 3
FROM (SELECT k,
             (ARRAY['page','ui','api','auth','billing','search','share','file','notify','admin','sync','system'])[1 + k % 12] AS cat,
             (ARRAY['view','click','submit','open','close','start','finish','fail','retry','create','update','delete',
                    'export','import','invite','login','logout','upgrade','downgrade','refund'])[1 + (k / 12) % 20] AS verb
      FROM generate_series(1, 400) AS g(k)) s;

CREATE TABLE app_releases (
    app_version text COLLATE "C" PRIMARY KEY,
    major smallint NOT NULL,
    minor smallint NOT NULL,
    patch smallint NOT NULL,
    channel text COLLATE "C" NOT NULL,
    released_on date NOT NULL,
    is_lts boolean NOT NULL
);
-- version index v in 0..239 <-> 'v<major>.<minor>.<patch>' (events use the same formula); every 7th is missing
INSERT INTO app_releases
SELECT 'v' || (5 + v / 60)::text || '.' || ((v / 4) % 15)::text || '.' || (v % 4)::text,
       (5 + v / 60)::int2, ((v / 4) % 15)::int2, (v % 4)::int2,
       CASE WHEN v % 4 = 3 THEN 'beta' ELSE 'stable' END,
       DATE '2023-06-01' + v * 3,
       (v / 4) % 15 = 0
FROM generate_series(0, 239) AS g(v)
WHERE (v * 37) % 7 <> 0;

-- tenants through CREATE TABLE AS (the CTAS receiver; its columns carry no NOT NULL)
CREATE TABLE tenants USING pgrcolumnar WITH (sorted_by = 'tenant_id') AS
SELECT x.t::int4 AS tenant_id,
       ((ARRAY['Acme','Blue','Bright','Cedar','Delta','Ember','Falcon','Granite','Harbor','Iris','Juniper','Keystone',
               'Lumen','Maple','Nimbus','Orbit','Pioneer','Quartz','River','Summit','Tidal','Union','Vertex','Willow'])[(1 + x.h1 % 24)::int]
        || ' ' ||
        (ARRAY['Labs','Systems','Works','Group','Studio','Partners','Health','Logistics','Media','Capital','Foods','Energy',
               'Robotics','Analytics','Retail','Learning','Motors','Design','Security','Travel','Bank','Cloud','Games','Legal'])[(1 + (x.h1 / 24) % 24)::int]
        || ' ' || x.t::text) COLLATE "C" AS tenant_name,
       (CASE WHEN x.h2 % 100 < 40 THEN 'free' WHEN x.h2 % 100 < 65 THEN 'starter' WHEN x.h2 % 100 < 83 THEN 'team'
             WHEN x.h2 % 100 < 95 THEN 'business' ELSE 'enterprise' END) COLLATE "C" AS plan,
       (ARRAY['software','retail','finance','healthcare','education','manufacturing','media','logistics','hospitality',
              'real estate','energy','legal','nonprofit','government','telecom','agriculture'])[(1 + x.h3 % 16)::int] COLLATE "C" AS industry,
       (ARRAY['us-east','us-west','eu-west','eu-central','ap-south','ap-northeast','sa-east'])[(1 + (x.h3 / 16) % 7)::int] COLLATE "C" AS region,
       (1 + ((x.h1 % 1000) * (x.h2 % 1000)) / 400)::int4 AS seats,
       ((x.h3 % 500000) / 100.0)::numeric(12,2) AS mrr,
       DATE '2019-01-01' + (x.h2 % 2200)::int4 AS created_on,
       x.h1 % 9 = 0 AS is_trial,
       CASE WHEN x.t > 100 AND x.h3 % 10 = 0 THEN (1 + x.h2 % (x.t - 1))::int4 END AS parent_tenant_id,
       ((x.h1 % 10007) / 10007.0)::float8 AS churn_risk
FROM (SELECT g.t,
             hashint8extended(g.t, 301) & 1073741823 AS h1,
             hashint8extended(g.t, 302) & 1073741823 AS h2,
             hashint8extended(g.t, 303) & 1073741823 AS h3
      FROM generate_series(1::int8, :n_tenants) AS g(t)) x;

\echo analytics fixture: app_users
CREATE TABLE app_users (
    user_id bigint NOT NULL,
    tenant_id integer NOT NULL,
    email text COLLATE "C" NOT NULL,
    display_name text COLLATE "C" NOT NULL,
    country_code text COLLATE "C" NOT NULL,
    locale text COLLATE "C" NOT NULL,
    role text COLLATE "C" NOT NULL,
    signup_at timestamp NOT NULL,
    is_active boolean NOT NULL,
    birth_year smallint,
    lifetime_value numeric(12,2) NOT NULL,
    last_login_on date
) USING pgrcolumnar WITH (sorted_by = 'user_id');
-- serial generation: the scratch file's row order (and so the load) is the same on every run
SET max_parallel_workers_per_gather = 0;
-- tenant of user u (events repeat this formula): a product of two uniforms, skewed toward low tenant ids
COPY (
  SELECT x.u AS user_id,
         x.tid AS tenant_id,
         'u' || x.u::text || '.' || d.word[(1 + x.h1 % 64)::int] || '@t' || x.tid::text || '.example.com' AS email,
         d.fname[(1 + (x.h1 >> 6) % 40)::int] || ' ' || d.lname[(1 + (x.h2 >> 6) % 50)::int] AS display_name,
         d.ctry[(1 + (((((x.h3 & 1023) * ((x.h3 >> 10) & 1023)) >> 10) * 40) >> 10))::int] AS country_code,
         d.loc[(1 + (x.h2 >> 12) % 12)::int] AS locale,
         CASE WHEN (x.h3 >> 20) % 100 < 2 THEN 'owner' WHEN (x.h3 >> 20) % 100 < 10 THEN 'admin'
              WHEN (x.h3 >> 20) % 100 < 90 THEN 'member' ELSE 'guest' END AS role,
         TIMESTAMP '2022-03-01 00:00:00' + ((x.h1 >> 4) % 94608000) * INTERVAL '1 second' AS signup_at,
         x.h2 % 10 <> 0 AS is_active,
         CASE WHEN (x.h3 >> 12) % 10 < 3 THEN NULL ELSE (1950 + (x.h1 >> 20) % 55)::int2 END AS birth_year,
         (((x.h2 >> 3) % 1000000) / 100.0)::numeric(12,2) AS lifetime_value,
         CASE WHEN x.h2 % 10 = 0 THEN NULL ELSE DATE '2025-01-01' + ((x.h2 >> 5) % 150)::int4 END AS last_login_on
  FROM (SELECT g.u,
               1 + (((((hashint8extended(g.u, 7) & 16777215) * (hashint8extended(g.u, 8) & 16777215)) >> 24) * :n_tenants) >> 24) AS tid,
               hashint8extended(g.u, 201) & 1073741823 AS h1,
               hashint8extended(g.u, 202) & 1073741823 AS h2,
               hashint8extended(g.u, 203) & 1073741823 AS h3
        FROM generate_series(1::int8, :n_users) AS g(u)) x
  CROSS JOIN (SELECT
      ARRAY['cloud','data','report','export','invoice','budget','team','sprint','roadmap','design','review','launch',
            'meeting','notes','contract','sales','pipeline','forecast','hiring','onboarding','security','audit','backup',
            'migration','api','webhook','billing','refund','pricing','plan','quarter','summary','dashboard','metrics',
            'retention','churn','campaign','email','calendar','deadline','priority','bug','feature','release','mobile',
            'desktop','integration','chat','repo','ticket','okr','goal','template','checklist','timeline','gantt',
            'kanban','archive','policy','vendor','expense','payroll','travel','training'] AS word,
      ARRAY['Ada','Ben','Chen','Dana','Eli','Fatima','Gus','Hana','Ivan','Jade','Kofi','Lena','Mateo','Nia','Omar',
            'Priya','Quinn','Rosa','Sven','Tara','Umar','Vera','Wei','Ximena','Yusuf','Zoe','Aiko','Bruno','Carla',
            'Dev','Erin','Felix','Gita','Hugo','Ines','Jonas','Kira','Luca','Maya','Noah'] AS fname,
      ARRAY['Abbott','Banerjee','Costa','Dubois','Eriksen','Fischer','Garcia','Hughes','Ito','Jensen','Kowalski',
            'Larsen','Moreau','Nakamura','Okafor','Petrov','Quispe','Rossi','Schmidt','Tanaka','Ueda','Varga','Weber',
            'Xu','Yilmaz','Zhang','Alvarez','Brennan','Cohen','Diaz','Evans','Ferreira','Gruber','Haddad','Ibrahim',
            'Jovanovic','Kim','Lopez','Mensah','Novak','Olsen','Park','Reyes','Silva','Torres','Usman','Vogel','Wang',
            'Young','Zimmer'] AS lname,
      ARRAY['US','DE','GB','FR','IN','BR','JP','CA','AU','NL','ES','IT','SE','PL','MX','KR','SG','CH','BE','AT',
            'DK','NO','FI','IE','PT','CZ','IL','ZA','AR','CL','CO','NZ','TR','RO','HU','GR','UA','ID','PH','VN'] AS ctry,
      ARRAY['en-US','en-GB','de-DE','fr-FR','es-ES','pt-BR','ja-JP','ko-KR','nl-NL','sv-SE','pl-PL','it-IT'] AS loc) d
) TO :'users_tsv';
RESET max_parallel_workers_per_gather;
COPY app_users FROM :'users_tsv';

\echo analytics fixture: events (generate, then load)
CREATE TABLE events (
    event_id bigint NOT NULL,
    occurred_at timestamp NOT NULL,
    occurred_on date NOT NULL,
    ingested_at timestamptz NOT NULL,
    tenant_id integer NOT NULL,
    user_id bigint,
    session_key text COLLATE "C" NOT NULL,
    seq_in_session smallint NOT NULL,
    event_type_id smallint NOT NULL,
    http_method text COLLATE "C" NOT NULL,
    platform text COLLATE "C" NOT NULL,
    app_version text COLLATE "C" NOT NULL,
    country_code text COLLATE "C" NOT NULL,
    page_path text COLLATE "C" NOT NULL,
    referrer text COLLATE "C",
    search_query text COLLATE "C",
    user_agent text COLLATE "C" NOT NULL,
    request_id text COLLATE "C" NOT NULL,
    message text COLLATE "C" NOT NULL,
    payload text COLLATE "C",
    stack_trace text COLLATE "C",
    status_code smallint NOT NULL,
    is_error boolean NOT NULL,
    is_bot boolean NOT NULL,
    consent boolean,
    retry_count smallint NOT NULL,
    duration_ms integer NOT NULL,
    bytes_out bigint NOT NULL,
    latency_ms double precision NOT NULL,
    cpu_pct real NOT NULL,
    amount numeric(12,2),
    currency text COLLATE "C",
    screen_w smallint NOT NULL,
    screen_h smallint NOT NULL,
    experiment_bucket smallint,
    client_ip bigint NOT NULL
) USING pgrcolumnar WITH (sorted_by = 'occurred_on, tenant_id, occurred_at');
-- Row i: 90 days from 2025-03-01, time increasing with i. Six 30-bit row hashes (p), a session every ~6 rows
-- (sess, q), session-stable user / platform / country / version / device hashes (r, z), the tenant of the user
-- (z, the app_users formula), derived small integers (z), the version string (v), then the columns.
SET max_parallel_workers_per_gather = 0;
COPY (
  SELECT v.i AS event_id,
         v.ts AS occurred_at,
         v.ts::date AS occurred_on,
         (v.ts + ((v.h5 >> 10) % 4000) * INTERVAL '1 millisecond') AT TIME ZONE 'UTC' AS ingested_at,
         v.tid AS tenant_id,
         CASE WHEN v.sess % 29 = 0 THEN NULL ELSE v.uid END AS user_id,
         'ses_' || lpad(v.sess::text, 9, '0') AS session_key,
         v.seq AS seq_in_session,
         v.kind AS event_type_id,
         d.meth[v.mi] AS http_method,
         d.plat[v.pidx] AS platform,
         v.ver AS app_version,
         d.ctry[v.cidx] AS country_code,
         CASE WHEN (v.h3 >> 10) % 100 < 55
              THEN '/' || d.sec[(1 + (v.h3 >> 17) % 16)::int] || '/' || d.act[(1 + (v.h3 >> 21) % 25)::int]
                   || d.tab[(1 + (v.h3 >> 26) % 3)::int]
              ELSE '/' || d.sec[(1 + v.eid % 16)::int] || '/' || d.ent[(1 + (v.eid / 16) % 8)::int] || '/' || v.eid::text
                   || '/' || d.act[(1 + (v.eid / 128) % 25)::int]
         END AS page_path,
         CASE WHEN (v.h4 >> 14) % 100 < 50 THEN ''
              WHEN (v.h4 >> 14) % 100 < 60 THEN NULL
              ELSE 'https://' || d.sub[(1 + (v.h5 >> 14) % 6)::int] || '.' || d.dom[(1 + (v.h5 >> 17) % 24)::int] || '.'
                   || d.tld[(1 + (v.h5 >> 22) % 4)::int] || '/' || d.sec[(1 + (v.h5 >> 24) % 16)::int] || '/'
                   || ((v.h2 >> 3) % 150000)::text || '?utm_source=' || d.src[(1 + (v.h2 >> 21) % 6)::int]
         END AS referrer,
         CASE WHEN (v.h1 >> 4) % 100 < 87 THEN NULL
              ELSE d.word[(1 + ((((((v.h4 >> 2) & 255) * ((v.h5 >> 2) & 255)) >> 8) * 64) >> 8))::int] || ' '
                   || d.word[(1 + (v.h2 >> 12) % 64)::int]
                   || CASE WHEN (v.h2 >> 25) % 3 = 0 THEN ' ' || d.word[(1 + (v.h1 >> 14) % 64)::int] ELSE '' END
         END AS search_query,
         CASE v.pidx
              WHEN 1 THEN 'Mozilla/5.0 (' || d.desk[(1 + (v.s1 >> 9) % 4)::int] || ') AppleWebKit/537.36 (KHTML, like Gecko) '
                          || d.browser[(1 + (v.s1 >> 11) % 5)::int] || '/' || (110 + (v.s1 >> 14) % 22)::text || '.0.'
                          || (((v.s1 >> 19) % 9) * 101)::text || '.0'
              WHEN 2 THEN 'PulseApp/' || v.ver || ' (iPhone; iOS ' || (16 + (v.s1 >> 9) % 3)::text || '.'
                          || ((v.s1 >> 12) % 6)::text || '; Scale/3.00)'
              WHEN 3 THEN 'PulseApp/' || v.ver || ' (Linux; Android ' || (12 + (v.s1 >> 9) % 4)::text || '; '
                          || d.phone[(1 + (v.s1 >> 12) % 6)::int] || ')'
              WHEN 4 THEN 'PulseDesktop/' || v.ver || ' (Macintosh; macOS 14.' || ((v.s1 >> 9) % 6)::text || ') Electron/29.1.0'
              WHEN 5 THEN 'PulseDesktop/' || v.ver || ' (Windows NT 10.0; Win64; x64) Electron/29.1.0'
              WHEN 6 THEN 'PulseDesktop/' || v.ver || ' (X11; Linux x86_64) Electron/29.1.0'
              ELSE 'pulse-sdk-' || d.sdk[(1 + (v.s1 >> 9) % 5)::int] || '/' || (2 + (v.s1 >> 13) % 3)::text || '.'
                   || ((v.s1 >> 15) % 10)::text || '.0 (+https://pulse.example.com/bot)'
         END AS user_agent,
         md5('req' || v.i::text) AS request_id,
         'req=' || v.i::text || ' ' || d.meth[v.mi] || ' ' || d.sec[(1 + (v.h2 >> 8) % 16)::int] || ' -> ' || v.sc::text
             || ' in ' || v.dur::text || 'ms'
             || CASE WHEN v.sc >= 500 THEN ' error=' || d.err[(1 + (v.h4 >> 26) % 4)::int]
                     WHEN v.retry > 0 THEN ' retries=' || v.retry::text ELSE '' END AS message,
         CASE WHEN (v.h5 >> 26) % 4 = 0
              THEN '{"v":2,"kind":' || v.kind::text || ',"ab":' || coalesce(v.bucket::text, 'null') || ',"flags":["'
                   || d.flag[(1 + (v.h1 >> 3) % 8)::int] || '","' || d.flag[(1 + (v.h2 >> 6) % 8)::int] || '"],"trace":"'
                   || substr(md5('t' || v.i::text), 1, 16) || '","retry":' || CASE WHEN v.retry > 0 THEN 'true' ELSE 'false' END || '}'
         END AS payload,
         CASE WHEN v.sc >= 500
              THEN repeat('at com.pulse.' || d.sec[(1 + (v.h2 >> 8) % 16)::int] || '.' || d.layer[(1 + (v.h2 >> 12) % 4)::int]
                          || '.run(' || d.layer[(1 + (v.h2 >> 14) % 4)::int] || '.java:' || (10 + v.h2 % 900)::text || ') | ',
                          CASE WHEN (v.h4 >> 22) % 20 = 0 THEN 200 ELSE 6 + ((v.h4 >> 22) % 30)::int END)
         END AS stack_trace,
         v.sc AS status_code,
         v.sc >= 500 AS is_error,
         (v.s1 >> 7) % 50 = 0 AS is_bot,
         CASE WHEN v.uid % 17 = 0 THEN NULL ELSE v.uid % 5 <> 0 END AS consent,
         v.retry AS retry_count,
         v.dur AS duration_ms,
         ((v.h5 >> 3) % 250000) * (1 + (v.h2 >> 20) % 16) + CASE WHEN (v.h3 >> 10) % 2000 = 0 THEN 10000000000 ELSE 0 END AS bytes_out,
         v.dur::float8 * 0.8731 + ((v.h4 >> 20) % 1000)::float8 / 997 AS latency_ms,
         (((v.h5 >> 10) % 10000) / 100.0)::real AS cpu_pct,
         CASE WHEN v.buy THEN (((v.h5 >> 3) % 99900) / 100.0 + 1)::numeric(12,2) END AS amount,
         CASE WHEN v.buy THEN d.cur[(1 + (v.h5 >> 22) % 4)::int] END AS currency,
         d.sw[(1 + (v.s1 >> 13) % 12)::int]::int2 AS screen_w,
         d.sh[(1 + (v.s1 >> 13) % 12)::int]::int2 AS screen_h,
         v.bucket AS experiment_bucket,
         167772160 + ((v.s2 >> 4) % 300000) * 7919 AS client_ip
  FROM (
    SELECT z.*,
           'v' || (5 + z.vi / 60)::text || '.' || ((z.vi / 4) % 15)::text || '.' || (z.vi % 4)::text AS ver
    FROM (
      SELECT r.*,
             (1 + (((((hashint8extended(r.uid, 7) & 16777215) * (hashint8extended(r.uid, 8) & 16777215)) >> 24) * :n_tenants) >> 24))::int4 AS tid,
             CASE WHEN r.s1 % 100 < 38 THEN 1 WHEN r.s1 % 100 < 60 THEN 2 WHEN r.s1 % 100 < 80 THEN 3 WHEN r.s1 % 100 < 88 THEN 4
                  WHEN r.s1 % 100 < 95 THEN 5 WHEN r.s1 % 100 < 97 THEN 6 ELSE 7 END AS pidx,
             CASE WHEN r.h2 % 100 < 70 THEN 1 WHEN r.h2 % 100 < 90 THEN 2 WHEN r.h2 % 100 < 95 THEN 3 WHEN r.h2 % 100 < 98 THEN 4
                  ELSE 5 END AS mi,
             (1 + (((((r.s2 & 1023) * ((r.s2 >> 10) & 1023)) >> 10) * 40) >> 10))::int AS cidx,
             239 - (((((r.s3 & 1023) * ((r.s3 >> 10) & 1023)) >> 10) * 240) >> 10) AS vi,
             (r.dur0 + CASE WHEN r.sc >= 500 THEN 5000 ELSE 0 END)::int4 AS dur,
             (CASE WHEN (r.h4 >> 20) % 50 = 0 THEN 1 + (r.h4 >> 26) % 4 ELSE 0 END)::int2 AS retry,
             (r.h5 >> 12) % 37 = 0 AS buy,
             (CASE WHEN r.sess % 2 = 0 THEN NULL ELSE (r.sess / 2) % 10 END)::int2 AS bucket
      FROM (
        SELECT q.*,
               1 + (((((hashint8extended(q.sess, 107) & 16777215) * (hashint8extended(q.sess, 108) & 16777215)) >> 24) * :n_users) >> 24) AS uid,
               hashint8extended(q.sess, 109) & 1073741823 AS s1,
               hashint8extended(q.sess, 110) & 1073741823 AS s2,
               hashint8extended(q.sess, 111) & 1073741823 AS s3,
               (CASE WHEN q.m3 < 880 THEN 200 WHEN q.m3 < 915 THEN 201 WHEN q.m3 < 930 THEN 204 WHEN q.m3 < 945 THEN 304
                     WHEN q.m3 < 955 THEN 302 WHEN q.m3 < 965 THEN 400 WHEN q.m3 < 972 THEN 401 WHEN q.m3 < 978 THEN 403
                     WHEN q.m3 < 988 THEN 404 WHEN q.m3 < 991 THEN 409 WHEN q.m3 < 994 THEN 429 WHEN q.m3 < 997 THEN 500
                     WHEN q.m3 < 999 THEN 502 ELSE 503 END)::int2 AS sc
        FROM (
          SELECT p.*,
                 p.sj / 6 AS sess,
                 (p.sj % 6)::int2 AS seq,
                 TIMESTAMP '2025-03-01 00:00:00' + ((p.i - 1) * 7776000 / :n_events) * INTERVAL '1 second'
                     + (p.h5 % 1000) * INTERVAL '1 millisecond' AS ts,
                 p.h3 % 1000 AS m3,
                 p.i / 8 + (p.h4 >> 4) % 40000 AS eid,
                 ((p.h4 & 1023) * ((p.h4 >> 10) & 1023)) >> 6 AS dur0,
                 (1 + ((((((p.h1 >> 20) & 1023) * (p.h2 & 1023)) >> 10) * 400) >> 10))::int2 AS kind
          FROM (
            SELECT g.i,
                   hashint8extended(g.i, 101) & 1073741823 AS h1,
                   hashint8extended(g.i, 102) & 1073741823 AS h2,
                   hashint8extended(g.i, 103) & 1073741823 AS h3,
                   hashint8extended(g.i, 104) & 1073741823 AS h4,
                   hashint8extended(g.i, 105) & 1073741823 AS h5,
                   g.i + (hashint8extended(g.i, 106) & 63) AS sj
            FROM generate_series(1::int8, :n_events) AS g(i)
          ) p
        ) q
      ) r
    ) z
  ) v
  CROSS JOIN (SELECT
      ARRAY['home','dashboard','projects','tasks','reports','settings','billing','search','inbox','calendar','files',
            'admin','integrations','analytics','help','onboarding'] AS sec,
      ARRAY['view','list','edit','create','delete','share','export','import','archive','restore','comment','assign',
            'filter','sort','preview','download','upload','invite','approve','reject','pin','duplicate','move','rename',
            'sync'] AS act,
      ARRAY['', '/details', '/history'] AS tab,
      ARRAY['project','task','report','file','doc','board','ticket','invoice'] AS ent,
      ARRAY['web','ios','android','macos','windows','linux','api'] AS plat,
      ARRAY['GET','POST','PUT','PATCH','DELETE'] AS meth,
      ARRAY['US','DE','GB','FR','IN','BR','JP','CA','AU','NL','ES','IT','SE','PL','MX','KR','SG','CH','BE','AT',
            'DK','NO','FI','IE','PT','CZ','IL','ZA','AR','CL','CO','NZ','TR','RO','HU','GR','UA','ID','PH','VN'] AS ctry,
      ARRAY['cloud','data','report','export','invoice','budget','team','sprint','roadmap','design','review','launch',
            'meeting','notes','contract','sales','pipeline','forecast','hiring','onboarding','security','audit','backup',
            'migration','api','webhook','billing','refund','pricing','plan','quarter','summary','dashboard','metrics',
            'retention','churn','campaign','email','calendar','deadline','priority','bug','feature','release','mobile',
            'desktop','integration','chat','repo','ticket','okr','goal','template','checklist','timeline','gantt',
            'kanban','archive','policy','vendor','expense','payroll','travel','training'] AS word,
      ARRAY['www','blog','docs','news','forum','m'] AS sub,
      ARRAY['acme','globex','initech','umbrella','hooli','vandelay','stark','wayne','wonka','tyrell','cyberdyne',
            'soylent','massive','piedpiper','aperture','blackmesa','oscorp','gringotts','monarch','duff','krusty',
            'sterling','prestige','dunder'] AS dom,
      ARRAY['com','net','org','io'] AS tld,
      ARRAY['newsletter','partner','ads','social','search','affiliate'] AS src,
      ARRAY['Windows NT 10.0; Win64; x64','Macintosh; Intel Mac OS X 10_15_7','X11; Linux x86_64','X11; CrOS x86_64 14541.0.0'] AS desk,
      ARRAY['Chrome','Edg','Firefox','Safari','OPR'] AS browser,
      ARRAY['Pixel 8','SM-S918B','SM-A546E','Redmi Note 12','moto g84','OnePlus 11'] AS phone,
      ARRAY['python','node','go','java','ruby'] AS sdk,
      ARRAY['timeout','upstream_reset','db_deadlock','oom_killed'] AS err,
      ARRAY['beta','dark','compact','offline','sso','export','ai','legacy'] AS flag,
      ARRAY['Handler','Service','Repository','Client'] AS layer,
      ARRAY['USD','EUR','GBP','JPY'] AS cur,
      ARRAY[390, 393, 412, 430, 768, 820, 1280, 1366, 1440, 1536, 1920, 2560] AS sw,
      ARRAY[844, 852, 915, 932, 1024, 1180, 800, 768, 900, 864, 1080, 1440] AS sh) d
) TO :'events_tsv';
RESET max_parallel_workers_per_gather;
COPY events FROM :'events_tsv';

\echo analytics fixture: events_heap (heap slice + btree indexes)
CREATE TABLE events_heap (LIKE events) USING heap;
SET max_parallel_workers_per_gather = 0;
INSERT INTO events_heap SELECT * FROM events WHERE event_id <= :n_heap;
RESET max_parallel_workers_per_gather;
CREATE UNIQUE INDEX events_heap_event_id ON events_heap (event_id);
CREATE INDEX events_heap_user_time ON events_heap (user_id, occurred_at);
CREATE INDEX events_heap_request_id ON events_heap (request_id);

\echo analytics fixture: statistics
VACUUM ANALYZE events_heap;
ANALYZE events;
ANALYZE app_users;
ANALYZE tenants;
ANALYZE event_kinds;
ANALYZE app_releases;
SELECT 'events' AS relation, count(*) AS row_count FROM events
UNION ALL SELECT 'app_users', count(*) FROM app_users
UNION ALL SELECT 'tenants', count(*) FROM tenants
UNION ALL SELECT 'event_kinds', count(*) FROM event_kinds
UNION ALL SELECT 'app_releases', count(*) FROM app_releases
UNION ALL SELECT 'events_heap', count(*) FROM events_heap;

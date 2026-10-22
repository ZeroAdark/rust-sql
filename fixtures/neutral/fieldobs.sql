-- fieldobs: the neutral test dataset (fixtures/neutral). A biodiversity field-survey log: observers walk survey
-- sites and record sightings of taxa, grouped into checklists (12 sightings each). The data is generated here, in
-- SQL, from generate_series and integer hashing (hashint8extended with fixed seeds, md5 of integers, fixed word
-- arrays): no random(), no clock, no downloaded data. The same :rows gives the same rows on PostgreSQL and on
-- pgrust, at any scale. It is not modeled on any benchmark's tables, columns or queries.
--
-- Text: sightings' text columns are COLLATE "C" (byte order on every server); every other table's text columns take
-- the database default, so both collation forms are in the dataset (observers: a columnar table with default-collation
-- text). Load it into a C database
-- (initdb --no-locale, or CREATE DATABASE ... TEMPLATE template0 LOCALE 'C') when the dimensions' text order matters.
-- Run it with psql as a superuser:
--   psql -v rows=20000 -f fixtures/neutral/fieldobs.sql                    pgrust: sightings and observers columnar
--   psql -v rows=20000 -v storage=heap -f fixtures/neutral/fieldobs.sql    every table heap (stock PostgreSQL too)
-- From another SQL file: \set rows 20000 then \ir <relative path>/fieldobs.sql.
--
-- Variables (psql -v):
--   rows     sightings rows (default 20000). Scales: 2000 (tiny), 20000 (small: plan capture, crate tests),
--            1000000 (medium), 10000000 (e2e). The dimensions scale with it where noted below.
--   storage  columnar (default: sightings and observers USING pgrcolumnar with a sorted_by key, loaded by
--            COPY TO + COPY FROM a server-side scratch file) or heap (every table heap, INSERT ... SELECT, no files).
--   scratch  columnar mode's server-side scratch directory (default /tmp); the files are emptied after the load.
--   keep_gen set it to keep the generator schema fieldobs_gen (views sightings_src, observers_src).
--
--   relation        storage                rows                      role
--   regions         heap, PK               64                        region dimension (int2 key)
--   sites           heap, PK               2,000                     survey sites (int4 key, self reference)
--   taxa            heap, PK               4,000                     species / taxon dimension (int4 key)
--   observers       columnar (observer_id) greatest(100, rows / 16)  people (63-bit hashed int8 key)
--   sightings       columnar (site_id,     rows                      the wide fact table, 37 columns
--                   observed_on, observed_at)
--   sightings_heap  heap + 4 btree         greatest(1, rows / 5)     sightings 1 .. rows / 5: index scans, nested loops
--
-- sightings column kinds (N = nullable; every other column is NOT NULL):
--   int8   sighting_id (unique, increasing with time), checklist_id (rows / 12 values, 12 consecutive rows each),
--          observer_id (63-bit hashes, skewed: a heavy head and a long tail), media_bytes N (NULL without media)
--   int4   site_id (2,000, skewed), taxon_id (4,000, skewed), cell_code (signed, full int4 range, ~site x 64),
--          duration_min (1..600, skewed)
--   int2   region_id (64), individual_count (1 on 70% of rows, up to 500), flag_reason (0 on ~97% of rows, 1..9),
--          quality_grade (0..4, skewed to 0), tz_offset_min (5 values), photo_width / photo_height (0 without
--          media, 8 sizes), elevation_m N (5% NULL)
--   bool   is_verified, has_media, is_private N
--   text   low cardinality: method (6), habitat (14), weather N (8; 25% NULL), device_model (80% '', ~320 values)
--          phrase-like: taxon_label (the common name, ~4,000, 4% ''), notes (82% '', 2-5 words, ~40,000 phrases)
--          URL-like: media_url (35% '', https://<8 hosts>/..., up to rows / 2 distinct),
--                    source_url (50% '', http or https, 25% www., 60 hosts, four path forms incl. a query string)
--          unique: upload_key (md5 of sighting_id)
--   time   observed_at timestamp (120 days from 2024-03-01, second resolution, increasing with checklist_id),
--          observed_on date, uploaded_at timestamptz (observed_at + 1 min .. 1 day, read as UTC)
--   numeric latitude / longitude numeric(9,6), temperature_c numeric(4,1) N, fee_paid numeric(8,2) N (97% NULL)
--   float  accuracy_m real N, distance_km float8
-- Skew is the product of two uniform hashes (a heavy head, a long tail). Ties: several rows share a sort key; a
-- test that needs a stable order orders by sighting_id last.
\set ON_ERROR_STOP 1
\if :{?rows}
\else
\set rows 20000
\endif
\if :{?storage}
\else
\set storage columnar
\endif
\if :{?scratch}
\else
\set scratch /tmp
\endif
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
SELECT :'storage' = 'columnar' AS fieldobs_columnar,
       (:rows + 11) / 12 AS fieldobs_checklists,
       greatest(100, :rows / 16) AS fieldobs_observers,
       greatest(1000, :rows / 2) AS fieldobs_media,
       greatest(1, :rows / 5) AS fieldobs_heap_rows,
       :'scratch' || '/fieldobs-sightings-' || pg_backend_pid() || '.tsv' AS fieldobs_sightings_file,
       :'scratch' || '/fieldobs-observers-' || pg_backend_pid() || '.tsv' AS fieldobs_observers_file
\gset
\if :fieldobs_columnar
-- pgrust serves the columnar AM under this name; a database made by PostgreSQL's initdb has no pg_am row for it yet
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar') THEN
        CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler;
    END IF;
END $$;
\set fieldobs_fact_am 'USING pgrcolumnar WITH (sorted_by = ''site_id, observed_on, observed_at'')'
\set fieldobs_observers_am 'USING pgrcolumnar WITH (sorted_by = ''observer_id'')'
\else
\set fieldobs_fact_am ''
\set fieldobs_observers_am ''
\endif

DROP TABLE IF EXISTS sightings_heap, sightings, observers, taxa, sites, regions;
DROP SCHEMA IF EXISTS fieldobs_gen CASCADE;
CREATE SCHEMA fieldobs_gen;

-- regions: 64 rows. tz_offset_min is a function of region_id (sightings repeats the formula).
CREATE TABLE regions (
    region_id smallint PRIMARY KEY,
    region_name text NOT NULL,
    country_code text NOT NULL,
    tz_offset_min smallint NOT NULL,
    area_km2 numeric(12,1) NOT NULL,
    is_coastal boolean NOT NULL
);
INSERT INTO regions
SELECT g.r,
       (ARRAY['North','South','East','West','Upper','Lower','Central','Outer'])[1 + (g.r - 1) % 8] || ' ' ||
       (ARRAY['Basin','Coast','Highlands','Valley','Plains','Delta','Forest','Lakes'])[1 + (g.r - 1) / 8],
       (ARRAY['AR','BR','CA','CL','CO','MX','PE','US'])[1 + h.h1 % 8],
       (-8 + (g.r - 1) / 13) * 60,
       (5000 + h.h2 % 2000000) / 10.0,
       h.h3 % 4 = 0
FROM generate_series(1, 64) AS g(r)
CROSS JOIN LATERAL (SELECT hashint8extended(g.r, 101) & 1073741823 AS h1,
                           hashint8extended(g.r, 102) & 1073741823 AS h2,
                           hashint8extended(g.r, 103) & 1073741823 AS h3) AS h;

-- sites: 2,000 rows. region_id, habitat, latitude, longitude and elevation are functions of site_id through the
-- seeds 901 / 902 / 908 / 909 / 910 (sightings repeats them).
CREATE TABLE sites (
    site_id integer PRIMARY KEY,
    region_id smallint NOT NULL,
    site_name text NOT NULL,
    habitat text NOT NULL,
    established_on date NOT NULL,
    area_ha numeric(10,2) NOT NULL,
    is_protected boolean NOT NULL,
    parent_site_id integer,
    latitude numeric(9,6) NOT NULL,
    longitude numeric(9,6) NOT NULL,
    elevation_m smallint NOT NULL,
    site_url text NOT NULL
);
INSERT INTO sites
SELECT g.s,
       1 + h.s1 % 64,
       (ARRAY['Cedar','Willow','Heron','Fox','Granite','Silver','Maple','Aspen','Elk','Juniper','Otter','Falcon',
              'Larch','Hollow','Bramble','Sable','Linden','Osprey','Hazel','Pine','Alder','Bracken','Tamarack','Wren'])
           [1 + (g.s - 1) % 24] || ' ' ||
       (ARRAY['Marsh','Ridge','Creek','Meadow','Bluff','Pond','Woods','Flats','Point','Hollow','Springs','Canyon'])
           [1 + ((g.s - 1) / 24) % 12] ||
       CASE WHEN g.s > 288 THEN ' ' || ((g.s - 1) / 288 + 1) ELSE '' END,
       (ARRAY['wetland','grassland','forest','woodland','scrub','desert','alpine','coastal','riparian','urban',
              'farmland','tundra','cave','lake'])[1 + h.s2 % 14],
       DATE '1950-01-01' + (h.s3 % 26000)::int,
       (50 + h.s4 % 10000000) / 100.0,
       h.s5 % 3 = 0,
       CASE WHEN g.s > 100 AND h.s6 % 5 = 0 THEN 1 + h.s7 % (g.s - 1) END,
       30 + (h.s8 % 20000000) / 1000000.0,
       -120 + (h.s9 % 50000000) / 1000000.0,
       h.s10 % 3500,
       CASE WHEN h.s11 % 5 < 2 THEN '' ELSE 'https://sites.fieldobs.test/s/' || g.s END
FROM generate_series(1, 2000) AS g(s)
CROSS JOIN LATERAL (SELECT hashint8extended(g.s, 901) & 1073741823 AS s1,
                           hashint8extended(g.s, 902) & 1073741823 AS s2,
                           hashint8extended(g.s, 903) & 1073741823 AS s3,
                           hashint8extended(g.s, 904) & 1073741823 AS s4,
                           hashint8extended(g.s, 905) & 1073741823 AS s5,
                           hashint8extended(g.s, 906) & 1073741823 AS s6,
                           hashint8extended(g.s, 907) & 1073741823 AS s7,
                           hashint8extended(g.s, 908) & 1073741823 AS s8,
                           hashint8extended(g.s, 909) & 1073741823 AS s9,
                           hashint8extended(g.s, 910) & 1073741823 AS s10,
                           hashint8extended(g.s, 911) & 1073741823 AS s11) AS h;

-- taxa: 4,000 rows. common_name is a function of taxon_id (sightings.taxon_label repeats it): the noun varies
-- fastest, so the frequent (low) taxon ids carry different nouns.
CREATE TABLE taxa (
    taxon_id integer PRIMARY KEY,
    scientific_name text NOT NULL,
    common_name text NOT NULL,
    family text NOT NULL,
    order_name text NOT NULL,
    conservation_status text,
    body_mass_g numeric(10,3),
    is_native boolean NOT NULL,
    first_described smallint
);
INSERT INTO taxa
SELECT t.t,
       initcap((ARRAY['acro','bra','cal','dra','eri','fal','gla','hal','ixo','lam','mel','nyc','ory','pha','rhy',
                      'sca','tur','ver','xan','zon'])[1 + t.genus_no % 20] ||
               (ARRAY['ceph','dact','gnath','lop','mys','nect','pter','rhin','sorex','stom','thrix','tonus','urus',
                      'zoon','phila','cola','dera'])[1 + (t.genus_no / 20) % 17]) || ' ' ||
       (ARRAY['alba','borealis','minor','major','vulgaris','montana','palustris','sylvatica','rufa','maculata',
              'aquatica','australis'])[1 + (t.t - 1) % 12],
       (ARRAY['Great','Lesser','Common','Northern','Southern','Eastern','Western','Little','Spotted','Striped',
              'Crested','Painted','Hooded','Ringed','Banded','Mountain','River','Marsh','Desert','Forest','Coastal',
              'Pale','Dusky','Golden'])[1 + ((t.t - 1) / 40) % 24] || ' ' ||
       (ARRAY['Blue','Red','Green','Grey','Black','White','Brown','Yellow','Olive','Rufous','Ash','Scarlet','Silver',
              'Copper'])[1 + ((t.t - 1) / 960) % 14] || ' ' ||
       (ARRAY['Heron','Warbler','Sparrow','Finch','Owl','Hawk','Plover','Tern','Wren','Thrush','Swift','Kingfisher',
              'Frog','Toad','Salamander','Turtle','Snake','Lizard','Beetle','Moth','Butterfly','Dragonfly','Bee',
              'Orchid','Fern','Sedge','Oak','Willow','Aster','Lily','Moss','Lichen','Bat','Vole','Shrew','Fox','Hare',
              'Deer','Otter','Trout'])[1 + (t.t - 1) % 40],
       initcap((ARRAY['acro','bra','cal','dra','eri','fal','gla','hal','ixo','lam','mel','nyc','ory','pha','rhy',
                      'sca','tur','ver','xan','zon'])[1 + (t.family_no * 7) % 20] ||
               (ARRAY['ceph','dact','gnath','lop','mys','nect','pter','rhin','sorex','stom','thrix','tonus','urus',
                      'zoon','phila','cola','dera'])[1 + (t.family_no / 20) % 17]) || 'idae',
       initcap((ARRAY['ceph','dact','gnath','lop','mys','nect','pter','rhin','sorex','stom','thrix','tonus','urus',
                      'zoon','phila','cola','dera'])[1 + (t.family_no / 9) % 17] ||
               (ARRAY['acro','bra','cal','dra','eri','fal','gla','hal','ixo','lam','mel','nyc','ory','pha','rhy',
                      'sca','tur','ver','xan','zon'])[1 + (t.family_no / 153) % 20]) || 'formes',
       CASE WHEN h.t1 % 10 < 3 THEN NULL
            ELSE (ARRAY['LC','LC','LC','LC','LC','NT','NT','VU','EN','CR','DD'])[1 + h.t2 % 11] END,
       CASE WHEN h.t3 % 5 = 0 THEN NULL ELSE (1 + h.t4 % 50000000) / 1000.0 END,
       h.t5 % 5 <> 0,
       CASE WHEN h.t6 % 10 = 0 THEN NULL ELSE 1758 + h.t7 % 263 END
FROM (SELECT g.n AS t, (g.n - 1) / 12 AS genus_no, ((g.n - 1) / 12) * 3 / 4 AS family_no
      FROM generate_series(1, 4000) AS g(n)) AS t
CROSS JOIN LATERAL (SELECT hashint8extended(t.t, 301) & 1073741823 AS t1,
                           hashint8extended(t.t, 302) & 1073741823 AS t2,
                           hashint8extended(t.t, 303) & 1073741823 AS t3,
                           hashint8extended(t.t, 304) & 1073741823 AS t4,
                           hashint8extended(t.t, 305) & 1073741823 AS t5,
                           hashint8extended(t.t, 306) & 1073741823 AS t6,
                           hashint8extended(t.t, 307) & 1073741823 AS t7) AS h;

-- observers: greatest(100, rows / 16) rows. observer_id is a 63-bit hash of observer_no (seed 920; sightings repeats
-- it), so the key is a large, unordered int8.
CREATE TABLE observers (
    observer_id bigint NOT NULL,
    observer_no integer NOT NULL,
    handle text NOT NULL,
    joined_on date NOT NULL,
    home_region_id smallint,
    skill_level text NOT NULL,
    is_staff boolean NOT NULL,
    profile_url text
) :fieldobs_observers_am;
CREATE VIEW fieldobs_gen.observers_src AS
SELECT hashint8extended(g.k, 920) & 9223372036854775807 AS observer_id,
       g.k::int AS observer_no,
       (ARRAY['kestrel','heron','wren','marten','otter','lynx','sedge','fern','tern','vole','finch','moth'])
           [1 + h.o1 % 12] || '_' || g.k AS handle,
       DATE '2015-01-01' + (h.o2 % 3300)::int AS joined_on,
       (CASE WHEN h.o3 % 10 = 0 THEN NULL ELSE 1 + h.o4 % 64 END)::smallint AS home_region_id,
       (ARRAY['novice','intermediate','advanced','expert'])
           [1 + (((((h.o5 & 16777215) * (h.o6 & 16777215)) >> 24) * 4) >> 24)] AS skill_level,
       h.o7 % 40 = 0 AS is_staff,
       CASE WHEN h.o8 % 3 = 0 THEN NULL ELSE 'https://people.fieldobs.test/u/' || g.k END AS profile_url
FROM generate_series(1::bigint, :fieldobs_observers::bigint) AS g(k)
CROSS JOIN LATERAL (SELECT hashint8extended(g.k, 921) & 1073741823 AS o1,
                           hashint8extended(g.k, 922) & 1073741823 AS o2,
                           hashint8extended(g.k, 923) & 1073741823 AS o3,
                           hashint8extended(g.k, 924) & 1073741823 AS o4,
                           hashint8extended(g.k, 925) & 1073741823 AS o5,
                           hashint8extended(g.k, 926) & 1073741823 AS o6,
                           hashint8extended(g.k, 927) & 1073741823 AS o7,
                           hashint8extended(g.k, 928) & 1073741823 AS o8) AS h;

-- sightings: the fact table. Row i belongs to checklist c = (i - 1) / 12 + 1; a checklist fixes the site, the
-- observer, the method, the weather, the device and the start time; the row fixes the rest.
CREATE TABLE sightings (
    sighting_id bigint NOT NULL,
    checklist_id bigint NOT NULL,
    observed_at timestamp NOT NULL,
    observed_on date NOT NULL,
    uploaded_at timestamptz NOT NULL,
    site_id integer NOT NULL,
    region_id smallint NOT NULL,
    observer_id bigint NOT NULL,
    taxon_id integer NOT NULL,
    cell_code integer NOT NULL,
    individual_count smallint NOT NULL,
    flag_reason smallint NOT NULL,
    quality_grade smallint NOT NULL,
    tz_offset_min smallint NOT NULL,
    photo_width smallint NOT NULL,
    photo_height smallint NOT NULL,
    elevation_m smallint,
    duration_min integer NOT NULL,
    media_bytes bigint,
    is_verified boolean NOT NULL,
    has_media boolean NOT NULL,
    is_private boolean,
    method text COLLATE "C" NOT NULL,
    habitat text COLLATE "C" NOT NULL,
    weather text COLLATE "C",
    device_model text COLLATE "C" NOT NULL,
    taxon_label text COLLATE "C" NOT NULL,
    notes text COLLATE "C" NOT NULL,
    media_url text COLLATE "C" NOT NULL,
    source_url text COLLATE "C" NOT NULL,
    upload_key text COLLATE "C" NOT NULL,
    latitude numeric(9,6) NOT NULL,
    longitude numeric(9,6) NOT NULL,
    temperature_c numeric(4,1),
    fee_paid numeric(8,2),
    accuracy_m real,
    distance_km double precision NOT NULL
) :fieldobs_fact_am;

CREATE VIEW fieldobs_gen.sightings_src AS
SELECT d.i AS sighting_id,
       d.c AS checklist_id,
       d.observed_at,
       d.observed_at::date AS observed_on,
       (d.observed_at + (60 + d.r2 % 86400) * INTERVAL '1 second') AT TIME ZONE 'UTC' AS uploaded_at,
       d.site_id::int AS site_id,
       (1 + d.s1 % 64)::smallint AS region_id,
       hashint8extended(d.observer_no, 920) & 9223372036854775807 AS observer_id,
       d.taxon_id::int AS taxon_id,
       (hashint8extended(d.site_id * 64 + d.r5 % 64, 26) >> 32)::int AS cell_code,
       (CASE WHEN d.r7 % 10 < 7 THEN 1
             ELSE 1 + (((((d.r8 & 16777215) * (d.r9 & 16777215)) >> 24) * 500) >> 24) END)::smallint AS individual_count,
       (CASE WHEN d.r10 % 100 < 97 THEN 0 ELSE 1 + d.r11 % 9 END)::smallint AS flag_reason,
       ((((((d.r12 & 16777215) * (d.r13 & 16777215)) >> 24) * 5) >> 24))::smallint AS quality_grade,
       ((-8 + (d.s1 % 64) / 13) * 60)::smallint AS tz_offset_min,
       (CASE WHEN d.has_media THEN (ARRAY[640,800,1024,1280,1920,2048,3024,4032])[1 + d.m3 % 8] ELSE 0 END)::smallint
           AS photo_width,
       (CASE WHEN d.has_media THEN (ARRAY[480,600,768,720,1080,1536,4032,3024])[1 + d.m3 % 8] ELSE 0 END)::smallint
           AS photo_height,
       (CASE WHEN d.r14 % 20 = 0 THEN NULL ELSE d.s10 % 3500 + d.r15 % 50 END)::smallint AS elevation_m,
       d.duration_min::int AS duration_min,
       CASE WHEN d.has_media THEN 20000 + d.m4 % 12000000 END AS media_bytes,
       d.r16 % 10 < 4 AS is_verified,
       d.has_media,
       CASE WHEN d.r17 % 10 = 0 THEN NULL ELSE d.r18 % 7 = 0 END AS is_private,
       (ARRAY['point count','transect','incidental','camera trap','acoustic','mist net'])
           [1 + (((((d.c5 & 16777215) * (d.c6 & 16777215)) >> 24) * 6) >> 24)] AS method,
       (ARRAY['wetland','grassland','forest','woodland','scrub','desert','alpine','coastal','riparian','urban',
              'farmland','tundra','cave','lake'])[1 + CASE WHEN d.r19 % 10 < 8 THEN d.s2 ELSE d.r20 END % 14] AS habitat,
       CASE WHEN d.c7 % 4 = 0 THEN NULL
            ELSE (ARRAY['clear','overcast','light rain','rain','fog','wind','snow','haze'])[1 + d.c8 % 8] END AS weather,
       CASE WHEN d.c9 % 5 <> 0 THEN ''
            ELSE (ARRAY['Lumen','Kestrel','Corvid','Taiga','Sable','Orion','Fathom','Brightline'])[1 + d.c10 % 8] || ' ' ||
                 (ARRAY['S','X','Pro','Mini','Field'])[1 + d.c11 % 5] || ' ' || (1 + d.c12 % 8) END AS device_model,
       CASE WHEN d.r21 % 25 = 0 THEN ''
            ELSE (ARRAY['Great','Lesser','Common','Northern','Southern','Eastern','Western','Little','Spotted','Striped',
                        'Crested','Painted','Hooded','Ringed','Banded','Mountain','River','Marsh','Desert','Forest',
                        'Coastal','Pale','Dusky','Golden'])[1 + ((d.taxon_id - 1) / 40) % 24] || ' ' ||
                 (ARRAY['Blue','Red','Green','Grey','Black','White','Brown','Yellow','Olive','Rufous','Ash','Scarlet',
                        'Silver','Copper'])[1 + ((d.taxon_id - 1) / 960) % 14] || ' ' ||
                 (ARRAY['Heron','Warbler','Sparrow','Finch','Owl','Hawk','Plover','Tern','Wren','Thrush','Swift',
                        'Kingfisher','Frog','Toad','Salamander','Turtle','Snake','Lizard','Beetle','Moth','Butterfly',
                        'Dragonfly','Bee','Orchid','Fern','Sedge','Oak','Willow','Aster','Lily','Moss','Lichen','Bat',
                        'Vole','Shrew','Fox','Hare','Deer','Otter','Trout'])[1 + (d.taxon_id - 1) % 40] END AS taxon_label,
       CASE WHEN d.r22 % 100 < 82 THEN ''
            ELSE (CASE WHEN d.p1 % 2 = 0 THEN initcap(d.w1) ELSE d.w1 END) || ' ' || d.w2 ||
                 CASE WHEN d.p2 % 4 >= 1 THEN ' ' || d.w3 ELSE '' END ||
                 CASE WHEN d.p2 % 4 >= 2 THEN ' ' || d.w4 ELSE '' END ||
                 CASE WHEN d.p2 % 4 >= 3 THEN ' ' || d.w5 ELSE '' END END AS notes,
       CASE WHEN NOT d.has_media THEN ''
            ELSE 'https://' ||
                 (ARRAY['media.fieldobs.test','img.fieldobs.test','cdn.trailcam.test','photos.wildarchive.test',
                        'audio.soundmap.test','files.atlasnotes.test','static.marshlog.test','uploads.ridgesurvey.test'])
                     [1 + d.m1 % 8] || '/m/' || (d.m % 97) || '/' || d.m || '/' || left(md5(d.m::text), 12) || '.' ||
                 (ARRAY['jpg','jpg','jpg','jpg','png','wav','mp4'])[1 + d.m2 % 7] END AS media_url,
       CASE WHEN d.r23 % 2 = 0 THEN ''
            ELSE CASE WHEN d.r24 % 4 = 0 THEN 'http' ELSE 'https' END || '://' ||
                 CASE WHEN d.r25 % 4 = 0 THEN 'www.' ELSE '' END ||
                 (ARRAY['atlas','field','trail','marsh','ridge','basin','delta','prairie','canyon','harbor'])
                     [1 + (d.host_k - 1) % 10] ||
                 (ARRAY['notes','survey','watch','count','log','journal'])[1 + (d.host_k - 1) / 10] || '.test' ||
                 CASE d.r26 % 4
                      WHEN 0 THEN '/checklist/' || d.c
                      WHEN 1 THEN '/site/' || d.site_id || '/day/' || d.observed_at::date
                      WHEN 2 THEN '/map?site=' || d.site_id || '&zoom=' || (8 + d.r27 % 8)
                      ELSE '/species/' || d.taxon_id || '/photos' END END AS source_url,
       md5(d.i::text) AS upload_key,
       30 + (d.s8 % 20000000) / 1000000.0 + (d.r28 % 20001 - 10000) / 1000000.0 AS latitude,
       -120 + (d.s9 % 50000000) / 1000000.0 + (d.r29 % 20001 - 10000) / 1000000.0 AS longitude,
       CASE WHEN d.r30 % 100 < 15 THEN NULL ELSE (d.r31 % 651 - 200) / 10.0 END AS temperature_c,
       CASE WHEN d.r32 % 100 < 97 THEN NULL ELSE (500 + d.r33 % 50000) / 100.0 END AS fee_paid,
       CASE WHEN d.r34 % 5 = 0 THEN NULL ELSE ((1 + d.r35 % 5000)::float8 / 100)::real END AS accuracy_m,
       (d.r36 % 2000000)::float8 / 1000 AS distance_km
FROM (
    -- the derived keys of each row and the hashes of its site (seeds 901..910, as in sites), media item and phrase
    SELECT b.*,
           TIMESTAMP '2024-03-01 00:00:00'
               + (b.base_s + ((b.i - 1) % 12) * 41 + b.r1 % 40) * INTERVAL '1 second' AS observed_at,
           hashint8extended(b.site_id, 901) & 1073741823 AS s1,
           hashint8extended(b.site_id, 902) & 1073741823 AS s2,
           hashint8extended(b.site_id, 908) & 1073741823 AS s8,
           hashint8extended(b.site_id, 909) & 1073741823 AS s9,
           hashint8extended(b.site_id, 910) & 1073741823 AS s10,
           hashint8extended(b.m, 41) & 1073741823 AS m1,
           hashint8extended(b.m, 42) & 1073741823 AS m2,
           hashint8extended(b.m, 43) & 1073741823 AS m3,
           hashint8extended(b.m, 44) & 1073741823 AS m4,
           hashint8extended(b.p, 51) & 1073741823 AS p1,
           hashint8extended(b.p, 52) & 1073741823 AS p2,
           (ARRAY['seen','heard','flying','perched','feeding','calling','singing','nesting','pair','juvenile','adult',
                  'male','female','flock','near','over','along','under','by','at','creek','trail','ridge','pond',
                  'meadow','canopy','shore','marsh','fence','road','bridge','burn','log','early','late','morning',
                  'dusk','briefly','distant','close','two','three','several','with','young','molting','foraging',
                  'resting'])[1 + (hashint8extended(b.p, 53) & 1073741823) % 48] AS w1,
           (ARRAY['seen','heard','flying','perched','feeding','calling','singing','nesting','pair','juvenile','adult',
                  'male','female','flock','near','over','along','under','by','at','creek','trail','ridge','pond',
                  'meadow','canopy','shore','marsh','fence','road','bridge','burn','log','early','late','morning',
                  'dusk','briefly','distant','close','two','three','several','with','young','molting','foraging',
                  'resting'])[1 + (hashint8extended(b.p, 54) & 1073741823) % 48] AS w2,
           (ARRAY['seen','heard','flying','perched','feeding','calling','singing','nesting','pair','juvenile','adult',
                  'male','female','flock','near','over','along','under','by','at','creek','trail','ridge','pond',
                  'meadow','canopy','shore','marsh','fence','road','bridge','burn','log','early','late','morning',
                  'dusk','briefly','distant','close','two','three','several','with','young','molting','foraging',
                  'resting'])[1 + (hashint8extended(b.p, 55) & 1073741823) % 48] AS w3,
           (ARRAY['seen','heard','flying','perched','feeding','calling','singing','nesting','pair','juvenile','adult',
                  'male','female','flock','near','over','along','under','by','at','creek','trail','ridge','pond',
                  'meadow','canopy','shore','marsh','fence','road','bridge','burn','log','early','late','morning',
                  'dusk','briefly','distant','close','two','three','several','with','young','molting','foraging',
                  'resting'])[1 + (hashint8extended(b.p, 56) & 1073741823) % 48] AS w4,
           (ARRAY['seen','heard','flying','perched','feeding','calling','singing','nesting','pair','juvenile','adult',
                  'male','female','flock','near','over','along','under','by','at','creek','trail','ridge','pond',
                  'meadow','canopy','shore','marsh','fence','road','bridge','burn','log','early','late','morning',
                  'dusk','briefly','distant','close','two','three','several','with','young','molting','foraging',
                  'resting'])[1 + (hashint8extended(b.p, 57) & 1073741823) % 48] AS w5
    FROM (
        -- skewed keys: 1 + ((u1 * u2) * n) for two 24-bit uniforms (seed pairs), a heavy head and a long tail
        SELECT a.*,
               1 + (((((a.c1 & 16777215) * (a.c2 & 16777215)) >> 24) * 2000) >> 24) AS site_id,
               1 + (((((a.c3 & 16777215) * (a.c4 & 16777215)) >> 24) * :fieldobs_observers) >> 24) AS observer_no,
               ((a.c - 1) * 10368000) / :fieldobs_checklists + a.c13 % 1800 AS base_s,
               1 + (((((a.c14 & 16777215) * (a.c15 & 16777215)) >> 24) * 600) >> 24) AS duration_min,
               1 + (((((a.r3 & 16777215) * (a.r4 & 16777215)) >> 24) * 4000) >> 24) AS taxon_id,
               a.r37 % 100 >= 35 AS has_media,
               1 + (((((a.r38 & 16777215) * (a.r39 & 16777215)) >> 24) * :fieldobs_media) >> 24) AS m,
               1 + (((((a.r40 & 16777215) * (a.r41 & 16777215)) >> 24) * 40000) >> 24) AS p,
               1 + (((((a.r42 & 16777215) * (a.r43 & 16777215)) >> 24) * 60) >> 24) AS host_k
        FROM (
            -- the checklist's hashes (c1..c15) and the row's (r1..r43), 30 bits each
            SELECT k.i, k.c,
                   hashint8extended(k.c, 1) & 1073741823 AS c1, hashint8extended(k.c, 2) & 1073741823 AS c2,
                   hashint8extended(k.c, 3) & 1073741823 AS c3, hashint8extended(k.c, 4) & 1073741823 AS c4,
                   hashint8extended(k.c, 5) & 1073741823 AS c5, hashint8extended(k.c, 6) & 1073741823 AS c6,
                   hashint8extended(k.c, 7) & 1073741823 AS c7, hashint8extended(k.c, 8) & 1073741823 AS c8,
                   hashint8extended(k.c, 9) & 1073741823 AS c9, hashint8extended(k.c, 10) & 1073741823 AS c10,
                   hashint8extended(k.c, 11) & 1073741823 AS c11, hashint8extended(k.c, 12) & 1073741823 AS c12,
                   hashint8extended(k.c, 13) & 1073741823 AS c13, hashint8extended(k.c, 14) & 1073741823 AS c14,
                   hashint8extended(k.c, 15) & 1073741823 AS c15,
                   hashint8extended(k.i, 101) & 1073741823 AS r1, hashint8extended(k.i, 102) & 1073741823 AS r2,
                   hashint8extended(k.i, 103) & 1073741823 AS r3, hashint8extended(k.i, 104) & 1073741823 AS r4,
                   hashint8extended(k.i, 105) & 1073741823 AS r5, hashint8extended(k.i, 107) & 1073741823 AS r7,
                   hashint8extended(k.i, 108) & 1073741823 AS r8, hashint8extended(k.i, 109) & 1073741823 AS r9,
                   hashint8extended(k.i, 110) & 1073741823 AS r10, hashint8extended(k.i, 111) & 1073741823 AS r11,
                   hashint8extended(k.i, 112) & 1073741823 AS r12, hashint8extended(k.i, 113) & 1073741823 AS r13,
                   hashint8extended(k.i, 114) & 1073741823 AS r14, hashint8extended(k.i, 115) & 1073741823 AS r15,
                   hashint8extended(k.i, 116) & 1073741823 AS r16, hashint8extended(k.i, 117) & 1073741823 AS r17,
                   hashint8extended(k.i, 118) & 1073741823 AS r18, hashint8extended(k.i, 119) & 1073741823 AS r19,
                   hashint8extended(k.i, 120) & 1073741823 AS r20, hashint8extended(k.i, 121) & 1073741823 AS r21,
                   hashint8extended(k.i, 122) & 1073741823 AS r22, hashint8extended(k.i, 123) & 1073741823 AS r23,
                   hashint8extended(k.i, 124) & 1073741823 AS r24, hashint8extended(k.i, 125) & 1073741823 AS r25,
                   hashint8extended(k.i, 126) & 1073741823 AS r26, hashint8extended(k.i, 127) & 1073741823 AS r27,
                   hashint8extended(k.i, 128) & 1073741823 AS r28, hashint8extended(k.i, 129) & 1073741823 AS r29,
                   hashint8extended(k.i, 130) & 1073741823 AS r30, hashint8extended(k.i, 131) & 1073741823 AS r31,
                   hashint8extended(k.i, 132) & 1073741823 AS r32, hashint8extended(k.i, 133) & 1073741823 AS r33,
                   hashint8extended(k.i, 134) & 1073741823 AS r34, hashint8extended(k.i, 135) & 1073741823 AS r35,
                   hashint8extended(k.i, 136) & 1073741823 AS r36, hashint8extended(k.i, 137) & 1073741823 AS r37,
                   hashint8extended(k.i, 138) & 1073741823 AS r38, hashint8extended(k.i, 139) & 1073741823 AS r39,
                   hashint8extended(k.i, 140) & 1073741823 AS r40, hashint8extended(k.i, 141) & 1073741823 AS r41,
                   hashint8extended(k.i, 142) & 1073741823 AS r42, hashint8extended(k.i, 143) & 1073741823 AS r43
            FROM (SELECT g.i, (g.i - 1) / 12 + 1 AS c FROM generate_series(1::bigint, :rows::bigint) AS g(i)) AS k
        ) AS a
    ) AS b
) AS d;

\if :fieldobs_columnar
COPY (SELECT * FROM fieldobs_gen.observers_src) TO :'fieldobs_observers_file';
COPY observers FROM :'fieldobs_observers_file';
COPY (SELECT 1 WHERE false) TO :'fieldobs_observers_file';
COPY (SELECT * FROM fieldobs_gen.sightings_src) TO :'fieldobs_sightings_file';
COPY sightings FROM :'fieldobs_sightings_file';
COPY (SELECT 1 WHERE false) TO :'fieldobs_sightings_file';
\else
INSERT INTO observers SELECT * FROM fieldobs_gen.observers_src;
INSERT INTO sightings SELECT * FROM fieldobs_gen.sightings_src;
\endif

-- sightings_heap: the first fifth of sightings in a heap with btree indexes (point and range lookups, index nested
-- loops, the heap twin of the columnar fact table).
CREATE TABLE sightings_heap (LIKE sightings) USING heap;
INSERT INTO sightings_heap SELECT * FROM sightings WHERE sighting_id <= :fieldobs_heap_rows ORDER BY sighting_id;
CREATE UNIQUE INDEX sightings_heap_id ON sightings_heap (sighting_id);
CREATE INDEX sightings_heap_observer_time ON sightings_heap (observer_id, observed_at);
CREATE INDEX sightings_heap_site_day ON sightings_heap (site_id, observed_on);
CREATE INDEX sightings_heap_upload_key ON sightings_heap (upload_key);

\if :{?keep_gen}
\else
DROP SCHEMA fieldobs_gen CASCADE;
\endif
ANALYZE regions;
ANALYZE sites;
ANALYZE taxa;
ANALYZE observers;
ANALYZE sightings;
ANALYZE sightings_heap;
RESET max_parallel_workers_per_gather;
RESET client_min_messages;

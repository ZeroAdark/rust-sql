-- regression: substr() of an lz4-compressed out-of-line value decompresses only
-- the requested prefix (C lz4_decompress_datum_slice), so 40 calls on a
-- 200 MB value finish well inside the timeout.
CREATE TABLE lzs (id int, p text COMPRESSION pglz, l text COMPRESSION lz4);
INSERT INTO lzs SELECT 1, repeat('abcdefgh', 25000000), repeat('abcdefgh', 25000000);
INSERT INTO lzs SELECT 2, NULL, string_agg(md5(g::text), '') FROM generate_series(1, 20000) g;
SELECT id, pg_column_compression(l) AS l_comp FROM lzs ORDER BY id;
SET statement_timeout = '20s';
SELECT sum(length(substr(p, 1, 10))) AS pglz_prefix FROM lzs, generate_series(1, 40) WHERE id = 1;
SELECT sum(length(substr(l, 1, 10))) AS lz4_prefix FROM lzs, generate_series(1, 40) WHERE id = 1;
SELECT sum(length(substr(l, 5, 3))) AS lz4_small_offset FROM lzs, generate_series(1, 40) WHERE id = 1;
RESET statement_timeout;
SELECT substr(l, 1, 20), substr(l, 199999990, 20), length(l) FROM lzs WHERE id = 1;
SELECT md5(substr(l, 1, 100000)) = md5(left(l, 100000)) AS same, substr(l, 31, 40), substr(l, 639990, 20) FROM lzs WHERE id = 2;
SELECT md5(substr(l::bytea, 1, 5000)) FROM lzs WHERE id = 2;

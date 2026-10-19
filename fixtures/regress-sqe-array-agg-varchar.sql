\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE cv (id int, x varchar(5)) USING pgrcolumnar2;
COPY cv FROM stdin;
1	abc
2	\N
\.
CREATE TABLE hv (id int, x varchar(5));
INSERT INTO hv VALUES (1, 'abc'), (2, NULL);
-- text form is identical on both servers
SELECT array_agg(x ORDER BY id) FROM cv;
-- the binary form: write the aggregate to a binary COPY file, then read the array header back
COPY (SELECT array_agg(x ORDER BY id) FROM cv) TO '/tmp/reg_aasc_cv.bin' (FORMAT binary);
COPY (SELECT array_agg(x ORDER BY id) FROM hv) TO '/tmp/reg_aasc_hv.bin' (FORMAT binary);
-- element type OID at bytes 33..36 of the file (19-byte header, 2-byte field count, 4-byte length, ndim, flags, elemtype)
SELECT 'columnar' AS src, get_byte(f, 33) * 256 * 256 * 256 + get_byte(f, 34) * 256 * 256 + get_byte(f, 35) * 256 + get_byte(f, 36) AS elemtype_oid
  FROM (SELECT pg_read_binary_file('/tmp/reg_aasc_cv.bin') f) s
UNION ALL
SELECT 'heap', get_byte(f, 33) * 256 * 256 * 256 + get_byte(f, 34) * 256 * 256 + get_byte(f, 35) * 256 + get_byte(f, 36)
  FROM (SELECT pg_read_binary_file('/tmp/reg_aasc_hv.bin') f) s;
-- decisive: load the columnar aggregate's binary COPY output into a varchar[] column
CREATE TABLE back (a varchar(5)[]);
COPY back FROM '/tmp/reg_aasc_cv.bin' (FORMAT binary);
SELECT a FROM back;
-- control: the same from the heap twin loads
COPY back FROM '/tmp/reg_aasc_hv.bin' (FORMAT binary);
SELECT count(*) FROM back;

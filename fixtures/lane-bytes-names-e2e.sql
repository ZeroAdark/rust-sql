-- Differential fixture for the bytes-names lane (regression,
-- #2): generated constraint / index / index-column names are clipped at
-- NAMEDATALEN-1 BYTES like C's makeObjectName / ChooseIndexColumnNames,
-- even inside a multibyte character in a SQL_ASCII database, and the
-- catalog stores the clipped bytes verbatim. Run via
-- scripts/lane-bytes-names-e2e.sh against the frozen expected captured
-- from C PostgreSQL 18.6 (octet_length pins the byte count; encode() pins
-- the bytes around the clip since psql renders them lossily).
\pset pager off
CREATE DATABASE bytes_names_ascii ENCODING 'SQL_ASCII' TEMPLATE template0;
\c bytes_names_ascii
-- #1: 57 a's + éé (61 bytes): the pkey name keeps 58 bytes of the table
-- name = 57 a's + the first byte of é.
CREATE TABLE "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé"(id int PRIMARY KEY);
SELECT octet_length(conname::text), encode(substr(conname::text, 55)::bytea, 'hex')
  FROM pg_constraint WHERE conrelid = '"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé"'::regclass ORDER BY 1, 2;
SELECT octet_length(relname::text), encode(substr(relname::text, 55)::bytea, 'hex')
  FROM pg_class WHERE left(relname::text, 57) = repeat('a', 57) ORDER BY 1, 2;
-- 56 a's + éé: the clip lands after the whole first é.
CREATE TABLE "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé"(id int PRIMARY KEY);
SELECT octet_length(conname::text), encode(substr(conname::text, 55)::bytea, 'hex')
  FROM pg_constraint WHERE conrelid = '"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé"'::regclass ORDER BY 1, 2;
-- The clipped index is a real index: usable, and dropped with its table.
-- (A duplicate-key error would quote the name: C emits the raw bytes, pgrust
-- renders the catalog name lossily -- the NameData-as-text display wall of
-- carve-ratifications.md section 11 -- so the message is not diffed here.)
INSERT INTO "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé" VALUES (1), (2);
SELECT count(*) FROM "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé" WHERE id = 2;
-- #2: duplicate index column "<61 a's>é": the second attname is the
-- original clipped to 62 bytes (mid-é) + '1'.
CREATE TABLE t("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé" int, x int);
CREATE INDEX i ON t("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé");
SELECT attnum, octet_length(attname::text), encode(substr(attname::text, 60)::bytea, 'hex')
  FROM pg_attribute WHERE attrelid = 'i'::regclass ORDER BY attnum;
-- ChooseIndexName (idx label) and a UNIQUE constraint (key label) over the
-- same long column: the addition is byte-clipped by makeObjectName.
CREATE TABLE u("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé" int, y int);
CREATE INDEX ON u("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé");
SELECT octet_length(relname::text), encode(substr(relname::text, 55)::bytea, 'hex')
  FROM pg_class WHERE left(relname::text, 6) = 'u_aaaa' ORDER BY 1, 2;
ALTER TABLE u ADD UNIQUE("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé", y);
SELECT octet_length(conname::text), encode(substr(conname::text, 55)::bytea, 'hex')
  FROM pg_constraint WHERE conrelid = 'u'::regclass ORDER BY 1, 2;
DROP TABLE "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaéé", t, u;
\c postgres
DROP DATABASE bytes_names_ascii;

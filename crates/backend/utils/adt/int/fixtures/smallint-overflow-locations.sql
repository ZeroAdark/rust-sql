\set VERBOSITY verbose
SELECT 1::int2 + 32767::int2;
SELECT 32768::int2;
SELECT -('-32768'::int2);
SELECT '-32768'::int2 - 1::int2;
SELECT 32767::int2 * 2::int2;
SELECT '-32768'::int2 / '-1'::int2;
SELECT abs('-32768'::int2);

--
-- Normalization re-lexes the query text with the current GUCs and the
-- scanner's errors surface (fill_in_constant_lengths).
--
DROP EXTENSION IF EXISTS pg_stat_statements;
CREATE EXTENSION pg_stat_statements;
SET pg_stat_statements.track = 'all';
SELECT pg_stat_statements_reset() IS NOT NULL AS t;

-- Both statements arrive in one message: the SELECT is parsed with
-- standard_conforming_strings on and normalized after the SET took effect.
SET standard_conforming_strings = off \; SELECT U&'d\0061t' AS b178_x15;
SELECT query FROM pg_stat_statements WHERE query LIKE '%b178_x15%';
SHOW standard_conforming_strings;

SELECT pg_stat_statements_reset() IS NOT NULL AS t;

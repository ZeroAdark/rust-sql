CREATE EXTENSION intarray;

-- _int_*_joinsel hand arraycontjoinsel the built-in operator, whose stub
-- answers DEFAULT_SEL without reading the other arguments, so SQL-declared
-- aliases return the constants.
CREATE FUNCTION int_overlap_joinsel_alias(int, int, int, int, int) RETURNS float8
  LANGUAGE C AS '$libdir/_int', '_int_overlap_joinsel';
CREATE FUNCTION int_contains_joinsel_alias(int, int, int, int, int) RETURNS float8
  LANGUAGE C AS '$libdir/_int', '_int_contains_joinsel';
CREATE FUNCTION int_contained_joinsel_alias(int, int, int, int, int) RETURNS float8
  LANGUAGE C AS '$libdir/_int', '_int_contained_joinsel';
SELECT int_overlap_joinsel_alias(1, 2, 3, 4, 5),
       int_contains_joinsel_alias(1, 2, 3, 4, 5),
       int_contained_joinsel_alias(1, 2, 3, 4, 5);

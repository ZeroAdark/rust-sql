\set VERBOSITY terse
DO $$
DECLARE
  original record;
  alternate ALIAS FOR original;
BEGIN
  original := ROW(1);
  PERFORM alternate.missing;
END $$;
DO $$
<<blk>>
DECLARE
  original record;
  alternate ALIAS FOR original;
BEGIN
  original := ROW(1);
  PERFORM blk.alternate.missing;
END $$;

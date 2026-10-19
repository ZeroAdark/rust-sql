-- batch-184 regression: ExecInitTableFuncScan compiles
-- the namespace expressions before the document expression
-- (nodeTableFuncscan.c:177-190), so the first EXECUTE privilege failure is
-- the namespace function's.
CREATE ROLE bugs184_user LOGIN;
CREATE FUNCTION bugs184_ns() RETURNS text LANGUAGE sql AS $$ SELECT 'http://x' $$;
CREATE FUNCTION bugs184_doc() RETURNS xml LANGUAGE sql AS $$ SELECT NULL::xml $$;
REVOKE EXECUTE ON FUNCTION bugs184_ns(), bugs184_doc() FROM public;
SET ROLE bugs184_user;
SELECT * FROM XMLTABLE(XMLNAMESPACES(bugs184_ns() AS x), '/a' PASSING bugs184_doc() COLUMNS c text);
SELECT * FROM XMLTABLE(XMLNAMESPACES(bugs184_ns() AS x), '/a' PASSING bugs184_doc() COLUMNS c text DEFAULT bugs184_ns());
RESET ROLE;
DROP FUNCTION bugs184_ns(), bugs184_doc();
DROP ROLE bugs184_user;

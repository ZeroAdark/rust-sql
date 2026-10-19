-- bugs/batch-230-mixed-lowtail, regression: an extension
-- script's plannable statements run through pg_plan_queries, so
-- debug_print_plan dumps their plans into the server log (checked by
-- scripts/bugs-batch230-e2e.sh; the client sees nothing at WARNING).
\getenv b230_ext B230_EXT_DIR
\set b230_ecp '$system:' :b230_ext
SET extension_control_path = :'b230_ecp';
SET debug_print_plan = on;
CREATE EXTENSION b230ext;
DROP EXTENSION b230ext;

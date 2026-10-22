CREATE EXTENSION pg_visibility;

-- SRF_FIRSTCALL_INIT runs before the relation is opened (pg_visibility.c:190,
-- 234, 334, 366): a scalar-declared entry point fails with 0A000 even on a view.
CREATE TABLE vt(a int);
INSERT INTO vt VALUES (1);
CREATE VIEW vv AS SELECT 1;
CREATE FUNCTION frozen_scalar(regclass) RETURNS record
  AS '$libdir/pg_visibility', 'pg_check_frozen' LANGUAGE C STRICT;
SELECT frozen_scalar('vv');
SELECT frozen_scalar('vt');
CREATE FUNCTION visible_scalar(regclass) RETURNS record
  AS '$libdir/pg_visibility', 'pg_check_visible' LANGUAGE C STRICT;
SELECT visible_scalar('vv');
CREATE FUNCTION vis_scalar(regclass) RETURNS record
  AS '$libdir/pg_visibility', 'pg_visibility_rel' LANGUAGE C STRICT;
SELECT vis_scalar('vv');
CREATE FUNCTION vism_scalar(regclass) RETURNS record
  AS '$libdir/pg_visibility', 'pg_visibility_map_rel' LANGUAGE C STRICT;
SELECT vism_scalar('vv');

-- pg_visibility_map_summary resolves a RECORD result from the caller's column
-- definition list (get_call_result_type(fcinfo, ...), pg_visibility.c:309).
CREATE FUNCTION vms(regclass) RETURNS record
  AS '$libdir/pg_visibility', 'pg_visibility_map_summary' LANGUAGE C STRICT;
SELECT * FROM vms('vt') AS (all_visible bigint, all_frozen bigint);
SELECT * FROM vms('vv') AS (all_visible bigint, all_frozen bigint);

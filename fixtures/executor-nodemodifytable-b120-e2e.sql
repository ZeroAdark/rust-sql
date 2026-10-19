-- bugs/batch-120-backend-executor-nodemodifytable: C-vs-pgrust parity for
-- the ModifyTable fixes. Expected file captured from C 18.6
-- (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- regression: a routed ON CONFLICT DO UPDATE checks
-- the UPDATE RLS WITH CHECK policies before the tuple constraints
-- (ExecUpdateAct), so a row violating both raises 42501, not 23514.
CREATE ROLE b120_u;
CREATE TABLE b120_rls(id int PRIMARY KEY, v int CHECK (v > 0)) PARTITION BY RANGE (id);
CREATE TABLE b120_rls1 PARTITION OF b120_rls FOR VALUES FROM (0) TO (10);
INSERT INTO b120_rls VALUES (1, 1);
ALTER TABLE b120_rls ENABLE ROW LEVEL SECURITY;
CREATE POLICY b120_sel ON b120_rls FOR SELECT USING (true);
CREATE POLICY b120_ins ON b120_rls FOR INSERT WITH CHECK (true);
CREATE POLICY b120_upd ON b120_rls FOR UPDATE USING (true) WITH CHECK (v > 0);
GRANT SELECT, INSERT, UPDATE ON b120_rls TO b120_u;
SET ROLE b120_u;
INSERT INTO b120_rls VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = -1;
INSERT INTO b120_rls VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = -1 WHERE false;
INSERT INTO b120_rls VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = 5;
SELECT * FROM b120_rls;
RESET ROLE;
-- non-partitioned control: same order in the plain UPDATE path.
CREATE TABLE b120_rlsn(id int PRIMARY KEY, v int CHECK (v > 0));
INSERT INTO b120_rlsn VALUES (1, 1);
ALTER TABLE b120_rlsn ENABLE ROW LEVEL SECURITY;
CREATE POLICY b120_seln ON b120_rlsn FOR SELECT USING (true);
CREATE POLICY b120_insn ON b120_rlsn FOR INSERT WITH CHECK (true);
CREATE POLICY b120_updn ON b120_rlsn FOR UPDATE USING (true) WITH CHECK (v > 0);
GRANT SELECT, INSERT, UPDATE ON b120_rlsn TO b120_u;
SET ROLE b120_u;
INSERT INTO b120_rlsn VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = -1;
RESET ROLE;
-- without the policy the constraint is what fires (23514).
DROP POLICY b120_upd ON b120_rls;
CREATE POLICY b120_upd ON b120_rls FOR UPDATE USING (true) WITH CHECK (true);
SET ROLE b120_u;
INSERT INTO b120_rls VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = -1;
RESET ROLE;
DROP TABLE b120_rls, b120_rlsn;
DROP ROLE b120_u;

-- regression: virtual generated NOT NULL columns are checked
-- in the per-tuple context (ExecRelGenVirtualNotNull); the multi-row
-- INSERT/UPDATE/upsert paths and the violation report are unchanged.
CREATE TABLE b120_vnn(a text, v text GENERATED ALWAYS AS (repeat(a, 100)) VIRTUAL NOT NULL);
INSERT INTO b120_vnn(a) SELECT 'x' || g FROM generate_series(1, 2000) g;
SELECT count(*), sum(length(v)) FROM b120_vnn;
INSERT INTO b120_vnn(a) VALUES ('y'), (NULL);
UPDATE b120_vnn SET a = a || 'u' WHERE a LIKE 'x1%';
SELECT count(*), sum(length(v)) FROM b120_vnn;
UPDATE b120_vnn SET a = NULL WHERE a = 'x5u';
CREATE TABLE b120_vnnp(id int PRIMARY KEY, a text, v text GENERATED ALWAYS AS (repeat(a, 10)) VIRTUAL NOT NULL) PARTITION BY RANGE (id);
CREATE TABLE b120_vnnp1 PARTITION OF b120_vnnp FOR VALUES FROM (0) TO (100000);
INSERT INTO b120_vnnp(id, a) SELECT g, 'p' FROM generate_series(1, 1000) g;
INSERT INTO b120_vnnp(id, a) SELECT g, 'q' FROM generate_series(1, 1000) g ON CONFLICT (id) DO UPDATE SET a = excluded.a;
SELECT count(*), sum(length(v)), min(a), max(a) FROM b120_vnnp;
INSERT INTO b120_vnnp(id, a) VALUES (1, 'r') ON CONFLICT (id) DO UPDATE SET a = NULL;
INSERT INTO b120_vnnp(id, a) VALUES (7, NULL);
DROP TABLE b120_vnn, b120_vnnp;

-- bugs/batch-228-mixed-lowtail: C-vs-pgrust parity. Expected file captured
-- from C 18.6 (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- regression: the LOG for an autosummarization request the autovacuum
-- work-item queue could not record carries SQLSTATE 54000 (brin.c:412).
-- 256 work items fill at pages_per_range = 1 once the heap passes 256 pages
-- (autovacuum is off in this harness, so nothing drains the queue).
SET client_min_messages = log;
CREATE TABLE b228_brin(i int);
CREATE INDEX b228_brin_idx ON b228_brin USING brin(i) WITH (pages_per_range = 1, autosummarize = on);
INSERT INTO b228_brin SELECT g FROM generate_series(1, 59000) g;
RESET client_min_messages;
SELECT count(*) FROM b228_brin WHERE i BETWEEN 100 AND 58999;
DROP TABLE b228_brin;

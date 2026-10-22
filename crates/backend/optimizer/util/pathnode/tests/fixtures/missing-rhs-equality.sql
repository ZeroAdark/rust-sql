BEGIN;
CREATE SCHEMA unique_rhs_test;
CREATE OPERATOR unique_rhs_test.=== (LEFTARG = integer, RIGHTARG = bigint, FUNCTION = int48eq, MERGES);
CREATE OPERATOR unique_rhs_test.<<< (LEFTARG = bigint, RIGHTARG = bigint, FUNCTION = int8lt);
CREATE OPERATOR FAMILY unique_rhs_test.missing_rhs_eq USING btree;
ALTER OPERATOR FAMILY unique_rhs_test.missing_rhs_eq USING btree ADD
    OPERATOR 3 unique_rhs_test.=== (integer, bigint),
    OPERATOR 1 unique_rhs_test.<<< (bigint, bigint);
CREATE TABLE unique_rhs_test.lhs (a integer);
CREATE TABLE unique_rhs_test.rhs (b bigint);
INSERT INTO unique_rhs_test.lhs VALUES (1), (2);
INSERT INTO unique_rhs_test.rhs VALUES (1), (1);
DO $$
BEGIN
    PERFORM a FROM unique_rhs_test.lhs
    WHERE a OPERATOR(unique_rhs_test.===) ANY (SELECT b FROM unique_rhs_test.rhs);
    RAISE EXCEPTION 'missing RHS equality was accepted';
EXCEPTION WHEN internal_error THEN
    RAISE NOTICE 'SQLSTATE: %, expected message: %', SQLSTATE,
        SQLERRM = format('could not find equality operator for ordering operator %s',
                         'unique_rhs_test.<<<(bigint,bigint)'::regoperator::oid);
END;
$$;
SELECT a FROM unique_rhs_test.lhs WHERE a = ANY (SELECT b FROM unique_rhs_test.rhs) ORDER BY a;
ROLLBACK;

# LOCK (pgrc delta-store, 2026-10-03): EvalPlanQual of SELECT ... FOR UPDATE / SHARE over a pgrcolumnar table under
# READ COMMITTED, as PG's eval-plan-qual does on heap: a locker that waited for a committed update rechecks its quals
# on the newest version (kept, or dropped once the update made the qual false), a join with a heap table rechecks the
# join, FOR UPDATE OF one side locks only that side, a chain of two updates is followed to its end, and a lock taken
# inside a function and a cursor. Rows: accounts 'checking' in a sealed part, 'savings' in the live segment. Expected
# output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE accounts (accountid text, balance numeric NOT NULL) USING pgrcolumnar; }
setup { INSERT INTO accounts VALUES ('checking', 600); }
setup { CALL pgrc_seal('accounts'); }
setup { INSERT INTO accounts VALUES ('savings', 600); }
setup { CREATE TABLE names (accountid text, owner text); INSERT INTO names VALUES ('checking', 'ann'), ('savings', 'bob'); }

teardown { DROP TABLE accounts; DROP TABLE names; }

session s1
step s1b    { BEGIN; }
step s1wc   { UPDATE accounts SET balance = balance - 200 WHERE accountid = 'checking'; }
step s1ws   { UPDATE accounts SET balance = balance - 200 WHERE accountid = 'savings'; }
step s1w2   { UPDATE accounts SET balance = balance + 7 WHERE accountid = 'checking'; }
step s1dc   { DELETE FROM accounts WHERE accountid = 'checking'; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2b    { BEGIN; }
step s2q    { SELECT * FROM accounts WHERE balance > 500 ORDER BY accountid FOR UPDATE; }
step s2qs   { SELECT * FROM accounts WHERE balance >= 400 ORDER BY accountid FOR SHARE; }
step s2j    { SELECT a.accountid, a.balance, n.owner FROM accounts a JOIN names n USING (accountid) WHERE a.balance > 500 ORDER BY 1 FOR UPDATE; }
step s2jo   { SELECT a.accountid, a.balance, n.owner FROM accounts a JOIN names n USING (accountid) ORDER BY 1 FOR UPDATE OF a; }
step s2cur  { DECLARE c CURSOR FOR SELECT * FROM accounts ORDER BY accountid FOR UPDATE; FETCH ALL FROM c; }
step s2u    { UPDATE accounts SET balance = balance * 2 WHERE accountid = 'checking' RETURNING *; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT * FROM accounts ORDER BY accountid; }

# the update makes the qual false: the row is skipped
permutation s1b s1wc s2b s2q s1c s2c rsel
permutation s1b s1ws s2b s2q s1c s2c rsel
# the qual still holds: the newest version is returned, locked
permutation s1b s1wc s2b s2qs s1c s2c rsel
# rolled back: the original row
permutation s1b s1ws s2b s2q s1a s2c rsel
# two updates in one transaction: followed to the end
permutation s1b s1wc s1w2 s2b s2qs s1c s2c rsel
# deleted: skipped
permutation s1b s1dc s2b s2qs s1c s2c rsel
# a join with a heap table, all sides locked, and FOR UPDATE OF the pgrcolumnar side only
permutation s1b s1wc s2b s2j s1c s2c rsel
permutation s1b s1ws s2b s2jo s1c s2c rsel
# a cursor
permutation s1b s1wc s2b s2cur s1c s2c rsel
# the locker first: the writer waits, then (READ COMMITTED) updates the locker's newest version
permutation s2b s2q s1b s1wc s2u s2c s1c rsel

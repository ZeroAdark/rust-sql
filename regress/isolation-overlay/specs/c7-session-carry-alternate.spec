# C7 (D3919): two sessions whose session GUCs differ (lc_monetary, read by to_char's 'L' on a pool thread, and
# quote_all_identifiers, read by the pool-hosted quote_ident) alternate their width-8 statements over the same Gravity
# pool threads: each claim applies its own leader's values (guc carry.rs `arm_claim`, the claim thread's plan of the
# leader's query pin) and puts the thread's own back at its end, so no claim reads the other session's. One session
# also changes a value inside a transaction (SET LOCAL, then ROLLBACK) between the other's statements, and one SETs and
# RESETs between its own. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE c7a (id int NOT NULL, n numeric, w text);
  INSERT INTO c7a SELECT g, g * 1.25 - 50000, 'w' || (g % 11) || CASE WHEN g % 3 = 0 THEN 'x' ELSE '' END FROM generate_series(1, 300000) g;
  ANALYZE c7a;
}
teardown { DROP TABLE c7a; }

session s1
setup		{ SET max_parallel_workers_per_gather = 8; SET lc_monetary = 'en_US.utf8'; SET quote_all_identifiers = on; }
step s1q	{ SELECT count(*), min(to_char(n, 'L999999990.00')) AS money, max(quote_ident(w)) AS ident FROM c7a WHERE id % 3 = 0; }
step s1b	{ BEGIN; SET LOCAL lc_monetary = 'C'; SET LOCAL quote_all_identifiers = off; }
step s1r	{ ROLLBACK; }

session s2
setup		{ SET max_parallel_workers_per_gather = 8; SET lc_monetary = 'C'; }
step s2q	{ SELECT count(*), min(to_char(n, 'L999999990.00')) AS money, max(quote_ident(w)) AS ident FROM c7a WHERE id % 3 = 0; }
step s2s	{ SET lc_monetary = 'en_US.utf8'; SET quote_all_identifiers = on; }
step s2t	{ RESET lc_monetary; RESET quote_all_identifiers; }

permutation s1q s2q s1q s2q s1q
permutation s1q s2s s2q s1q s2t s2q s1q
permutation s1b s1q s2q s1q s1r s1q s2q

# gravfix3 D3842: Hibernate's XmlHolder UPDATE puts an InitPlan (an uncorrelated subquery, attached at the ModifyTable)
# beside correlated SubPlans in one SET. Here it rechecks rows that another session updated or deleted. EvalPlanQual
# runs the correlated SubPlans again over the new row version and copies the InitPlan's value: an InitPlan that counts
# a table s1 inserts into still answers with the statement's count.

setup
{
  CREATE TABLE d3842x (id bigint PRIMARY KEY, aggregate xml);
  CREATE TABLE d3842o (id bigint PRIMARY KEY);
  INSERT INTO d3842x VALUES (1, '<e><gender>MALE</gender><theInt>1</theInt></e>'), (2, '<e><gender>FEMALE</gender><theInt>2</theInt></e>'), (3, NULL);
  INSERT INTO d3842o VALUES (1), (2);
}

teardown
{
  DROP TABLE d3842x;
  DROP TABLE d3842o;
}

session s1
step s1b { BEGIN; }
step s1u { UPDATE d3842x SET aggregate = '<e><gender>OTHER</gender><theInt>7</theInt></e>' WHERE id = 1; }
step s1n { UPDATE d3842x SET aggregate = NULL WHERE id = 2; }
step s1d { DELETE FROM d3842x WHERE id = 2; }
step s1o { INSERT INTO d3842o VALUES (3), (4), (5); }
step s1c { COMMIT; }
step s1r { ROLLBACK; }

session s2
step s2x { update d3842x xh1_0 set aggregate=xmlelement(name "e",xmlconcat(xmlelement(name "theString",(select xmlagg(t.v order by t.i) from xmltable('/Collection/*' passing cast(null as xml) columns v xml path '.', i for ordinality)t)),(select xmlagg(t.v) from xmltable('/e/gender' passing xh1_0.aggregate columns v xml path '.')t),(select xmlagg(t.v) from xmltable('/e/theInt' passing xh1_0.aggregate columns v xml path '.')t))); }
step s2k { update d3842x xh1_0 set aggregate=xmlelement(name "e",xmlconcat(xmlelement(name "theString",(select xmlagg(t.v order by t.i) from xmltable('/Collection/*' passing cast(null as xml) columns v xml path '.', i for ordinality)t)),(select xmlagg(t.v) from xmltable('/e/gender' passing xh1_0.aggregate columns v xml path '.')t))) where xh1_0.id = 1; }
step s2n { update d3842x xh1_0 set aggregate=xmlelement(name "e",xmlelement(name "n",(select count(*) from d3842o)),(select xmlagg(t.v) from xmltable('/e/gender' passing xh1_0.aggregate columns v xml path '.')t)); }
step s2s { SELECT id, aggregate::text FROM d3842x ORDER BY id; }

permutation s1b s1u s2x s1c s2s
permutation s1b s1u s2x s1r s2s
permutation s1b s1d s2x s1c s2s
permutation s1b s1u s1n s2x s1c s2s
permutation s1b s1u s2k s1c s2s
permutation s1b s1u s1o s2n s1c s2s
permutation s1b s1o s1u s1c s2n s2s

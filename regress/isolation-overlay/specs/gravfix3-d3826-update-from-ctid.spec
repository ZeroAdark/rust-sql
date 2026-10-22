# gravfix3 D3826 (2): Hibernate's UPDATE across an association joins the target to itself by ctid (update comments set ..
# from comments c1_0 join users u1_0 on .. where .. and comments.ctid = c1_0.ctid): a Nested Loop over users whose
# inner Hash Join probes a build of comments c1_0 with the target's scan. Here it rechecks rows that another session
# updated, deleted or left alone. EvalPlanQual refetches c1_0's row at its junk ctid (the version the statement joined)
# and runs the join again over the target's newest version: an updated row's new ctid no longer equals c1_0's, so the
# row is skipped; a rolled-back change leaves the row to update.

setup
{
  CREATE TABLE d3826usr (id bigint PRIMARY KEY, name varchar(255));
  CREATE TABLE d3826cmt (id bigint PRIMARY KEY, text varchar(255), user_id bigint REFERENCES d3826usr);
  INSERT INTO d3826usr VALUES (1, 'a'), (2, 'b'), (3, 'c');
  INSERT INTO d3826cmt VALUES (10, 't10', 1), (11, 't11', 1), (12, 't12', 1), (20, 't20', 2), (30, 't30', 3);
  ANALYZE d3826usr;
  ANALYZE d3826cmt;
}

teardown
{
  DROP TABLE d3826cmt;
  DROP TABLE d3826usr;
}

session s1
step s1b { BEGIN; }
step s1u { UPDATE d3826cmt SET text = text || '+' WHERE id = 11; }
step s1d { DELETE FROM d3826cmt WHERE id = 12; }
step s1n { UPDATE d3826usr SET name = 'renamed' WHERE id = 1; }
step s1c { COMMIT; }
step s1r { ROLLBACK; }

session s2
step s2u { update d3826cmt set text='x' from d3826cmt c1_0 join d3826usr u1_0 on u1_0.id=c1_0.user_id where u1_0.id=1 and d3826cmt.ctid=c1_0.ctid; }
step s2x { update d3826cmt set text=c1_0.text || '/' || u1_0.name from d3826cmt c1_0 join d3826usr u1_0 on u1_0.id=c1_0.user_id where u1_0.id=1 and d3826cmt.ctid=c1_0.ctid returning d3826cmt.id, d3826cmt.text; }
step s2d { delete from d3826cmt using d3826cmt c1_0 join d3826usr u1_0 on u1_0.id=c1_0.user_id where u1_0.id=1 and d3826cmt.ctid=c1_0.ctid; }
step s2s { SELECT * FROM d3826cmt ORDER BY id; }

permutation s1b s1u s2u s1c s2s
permutation s1b s1u s2u s1r s2s
permutation s1b s1d s2u s1c s2s
permutation s1b s1u s1d s2x s1c s2s
permutation s1b s1n s1u s2x s1c s2s
permutation s1b s1u s2d s1c s2s

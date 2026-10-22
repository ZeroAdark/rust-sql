use mcx::{alloc_leak_in, MemoryContext};
use types_error::{PgError, ERRCODE_QUERY_CANCELED};
use types_nodes::list::NodeList;
use types_nodes::parsenodes::{Query, RTEKind, RangeTblEntry};
use types_nodes::primnodes::FromExpr;
use types_nodes::Node;

#[test]
fn pull_up_subqueries_honours_a_pending_cancel() {
    postgres_seams::check_for_interrupts::set(|| {
        Err(Box::new(
            PgError::error("canceling statement due to user request")
                .with_sqlstate(ERRCODE_QUERY_CANCELED),
        ))
    });
    let ctx = MemoryContext::new("pullup-cancel");
    let mcx = ctx.mcx();
    let sub = alloc_leak_in(
        mcx,
        Query {
            hasAggs: true,
            ..Query::default()
        },
    )
    .unwrap();
    let mut rte = Node::build::<RangeTblEntry>(mcx).unwrap();
    rte.rtekind = RTEKind::RTE_SUBQUERY;
    rte.subquery = Some(sub);
    let rte = rte.seal();
    let mut parse = Query {
        rtable: NodeList::make1(mcx, rte).unwrap(),
        jointree: Some(
            alloc_leak_in(
                mcx,
                FromExpr {
                    fromlist: NodeList::make1(mcx, Node::mk_range_tbl_ref(mcx, 1).unwrap())
                        .unwrap(),
                    quals: None,
                },
            )
            .unwrap(),
        ),
        ..Query::default()
    };
    let mut run = planner::run::PlannerRun::new(mcx);
    let error = planner::prepjointree::pull_up_subqueries(&mut run, &mut parse).unwrap_err();
    assert_eq!(error.sqlstate(), ERRCODE_QUERY_CANCELED);
    assert_eq!(error.message(), "canceling statement due to user request");
    assert_eq!(parse.rtable.len(), 1);
    assert_eq!(
        parse.rtable.nth(0).as_range_tbl_entry().unwrap().rtekind,
        RTEKind::RTE_SUBQUERY
    );
    assert_eq!(
        parse
            .jointree
            .unwrap()
            .fromlist
            .nth(0)
            .as_range_tbl_ref()
            .unwrap()
            .rtindex,
        1
    );
}

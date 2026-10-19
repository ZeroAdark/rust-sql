extern crate std;

use mcx::MemoryContext;
use types_nodes::list::NodeList;
use types_nodes::parsenodes::Query;
use types_nodes::primnodes::{Const, TargetEntry};
use types_nodes::Node;

use crate::{CleanQuerytext, EnableQueryId, JumbleQuery};

#[test]
fn clean_querytext_trims_and_offsets() {
    let src = "  SELECT 1;   ";
    let mut loc = -1;
    let mut len = 0;
    let out = CleanQuerytext(src, &mut loc, &mut len);
    assert_eq!(out, "SELECT 1;");
    assert_eq!(loc, 2);
    assert_eq!(len, 9);

    let src = "SELECT 1; SELECT 2";
    let mut loc = 10;
    let mut len = 8;
    let out = CleanQuerytext(src, &mut loc, &mut len);
    assert_eq!(out, "SELECT 2");
    assert_eq!(loc, 10);
}

fn const_query<'mcx>(mcx: mcx::Mcx<'mcx>, consttype: u32, location: i32) -> Query<'mcx> {
    let c = Node::mk(
        mcx,
        Const {
            consttype,
            consttypmod: -1,
            constcollid: 0,
            constlen: 4,
            constvalue: datum::Datum::from_i32(1),
            constisnull: false,
            constbyval: true,
            location,
        },
    )
    .unwrap();
    let tle = Node::mk(
        mcx,
        TargetEntry {
            expr: c,
            resno: 1,
            resname: None,
            ressortgroupref: 0,
            resorigtbl: 0,
            resorigcol: 0,
            resjunk: false,
        },
    )
    .unwrap();
    Query {
        commandType: types_nodes::nodes_enums::CmdType::CMD_SELECT,
        targetList: NodeList::make1(mcx, tle).unwrap(),
        ..Default::default()
    }
}

#[test]
fn queryid_stable_normalized_and_locations_recorded() {
    EnableQueryId();
    let ctx = MemoryContext::new_bump("qjumble-test");
    let mcx = ctx.mcx();

    let mut q1 = const_query(mcx, 23, 7);
    let mut q2 = const_query(mcx, 23, 7);
    let js1 = JumbleQuery(mcx, &mut q1).unwrap();
    let js2 = JumbleQuery(mcx, &mut q2).unwrap();
    assert_ne!(q1.queryId, 0);
    assert_eq!(q1.queryId, q2.queryId);
    assert_eq!(js1.clocations.len(), 1);
    assert_eq!(js1.clocations[0].location, 7);
    assert_eq!(js1.clocations[0].length, -1);
    assert!(!js1.clocations[0].squashed);
    assert!(!js1.clocations[0].extern_param);
    assert_eq!(js2.clocations.len(), 1);

    // Different const TYPE changes the id; different const LOCATION does not
    // (values/locations are normalized away).
    let mut q3 = const_query(mcx, 25, 7);
    JumbleQuery(mcx, &mut q3).unwrap();
    assert_ne!(q3.queryId, q1.queryId);
    let mut q4 = const_query(mcx, 23, 99);
    JumbleQuery(mcx, &mut q4).unwrap();
    assert_eq!(q4.queryId, q1.queryId);
}

#[test]
fn constraint_reset_default_tblspc_distinguishes_queryid() {
    EnableQueryId();
    let ctx = MemoryContext::new_bump("qjumble-test");
    let mcx = ctx.mcx();

    // C's generated jumble walks every un-ignored Constraint field, including
    // the transform-set reset_default_tblspc flag (parsenodes.h:2852).
    let constraint_query = |reset: bool| -> Query<'_> {
        let c = Node::mk(
            mcx,
            types_nodes::rawnodes::Constraint {
                contype: types_nodes::rawnodes::ConstrType::CONSTR_UNIQUE,
                reset_default_tblspc: reset,
                ..Default::default()
            },
        )
        .unwrap();
        Query {
            commandType: types_nodes::nodes_enums::CmdType::CMD_UTILITY,
            utilityStmt: Some(c),
            ..Default::default()
        }
    };

    let mut q_set = constraint_query(true);
    let mut q_unset = constraint_query(false);
    JumbleQuery(mcx, &mut q_set).unwrap();
    JumbleQuery(mcx, &mut q_unset).unwrap();
    assert_ne!(q_set.queryId, 0);
    assert_ne!(
        q_set.queryId, q_unset.queryId,
        "reset_default_tblspc must feed the jumble as C does"
    );
}

#[test]
fn grouping_set_simple_content_jumbles_as_intlist() {
    EnableQueryId();
    let ctx = MemoryContext::new_bump("qjumble-test");
    let mcx = ctx.mcx();

    // Post-analysis SIMPLE grouping sets carry Integer nodes in memory where
    // C carries an IntList (list_make1_int in transformGroupClause); the
    // jumble must emit C's byte stream (T_IntList tag + raw ints), not
    // T_List + per-Integer tags. Trip-wire: GroupingSet.kind is NOT jumbled,
    // so under a generic list walk SIMPLE and SETS content with the same ref
    // would collide — C-exact jumbling separates them. R1-F1
    // JUMBLE-GROUPING-SETS.
    let gsets_query = |kind| -> Query<'_> {
        let content = NodeList::make1(mcx, Node::mk_integer(mcx, 1).unwrap()).unwrap();
        let gs = Node::mk_grouping_set(mcx, kind, content, -1).unwrap();
        Query {
            commandType: types_nodes::nodes_enums::CmdType::CMD_SELECT,
            groupingSets: NodeList::make1(mcx, gs).unwrap(),
            ..Default::default()
        }
    };
    let mut q_simple =
        gsets_query(types_nodes::parsenodes::GroupingSetKind::GROUPING_SET_SIMPLE);
    let mut q_sets = gsets_query(types_nodes::parsenodes::GroupingSetKind::GROUPING_SET_SETS);
    JumbleQuery(mcx, &mut q_simple).unwrap();
    JumbleQuery(mcx, &mut q_sets).unwrap();
    assert_ne!(q_simple.queryId, 0);
    assert_ne!(
        q_simple.queryId, q_sets.queryId,
        "SIMPLE grouping-set content must jumble as C's IntList, not a generic node list"
    );
}

#[test]
fn jumble_buffer_overflow_folds() {
    EnableQueryId();
    let ctx = MemoryContext::new_bump("qjumble-test");
    let mcx = ctx.mcx();

    // >1024 bytes of jumbled material exercises the fold path.
    let mut list = NodeList::nil();
    for i in 0..400 {
        let c = Node::mk(
            mcx,
            Const {
                consttype: i,
                consttypmod: -1,
                constcollid: 0,
                constlen: 4,
                constvalue: datum::Datum::from_i32(0),
                constisnull: false,
                constbyval: true,
                location: -1,
            },
        )
        .unwrap();
        list.lappend(mcx, c).unwrap();
    }
    let mut q = Query {
        commandType: types_nodes::nodes_enums::CmdType::CMD_SELECT,
        targetList: list,
        ..Default::default()
    };
    JumbleQuery(mcx, &mut q).unwrap();
    assert_ne!(q.queryId, 0);
}

#[test]
fn nil_list_cell_jumbles_as_a_pending_null() {
    EnableQueryId();
    let ctx = MemoryContext::new_bump("qjumble-test");
    let mcx = ctx.mcx();

    // C's NIL is a NULL pointer: gram.y's list_make2($1, NIL) (func_table)
    // and BEGIN ATOMIC END store a NIL cell that _jumbleNode(NULL) folds into
    // pending_nulls; the port boxes it as an empty List node, which must not
    // emit a T_List tag of its own.
    // Two NIL cells: T_List's tag value (1) is also a one-null count, so a
    // single cell could not tell the two byte streams apart.
    let mut cells = NodeList::nil();
    cells.lappend(mcx, Node::mk_list(mcx, NodeList::nil()).unwrap()).unwrap();
    cells.lappend(mcx, Node::mk_list(mcx, NodeList::nil()).unwrap()).unwrap();
    cells.lappend(mcx, Node::mk_string(mcx, "x").unwrap()).unwrap();
    let mut q = Query {
        commandType: types_nodes::nodes_enums::CmdType::CMD_UTILITY,
        utilityStmt: Some(Node::mk_list(mcx, cells).unwrap()),
        ..Default::default()
    };
    let js = JumbleQuery(mcx, &mut q).unwrap();
    let bytes = &js.jumble[..js.jumble_len];
    let list_tag = (types_nodes::NodeTag::T_List as u16 as u32).to_ne_bytes();
    let mut expected = list_tag.to_vec();
    expected.extend_from_slice(&2u32.to_ne_bytes());
    expected.extend_from_slice(&(types_nodes::NodeTag::T_String as u16 as u32).to_ne_bytes());
    expected.extend_from_slice(b"x\0");
    assert!(
        bytes.windows(expected.len()).any(|w| w == expected.as_slice()),
        "outer T_List tag, two pending nulls flushed, then the tagged string"
    );
    let three_tags = [list_tag, list_tag, list_tag].concat();
    assert!(
        !bytes.windows(three_tags.len()).any(|w| w == three_tags.as_slice()),
        "an empty list cell must not add a T_List tag"
    );
}

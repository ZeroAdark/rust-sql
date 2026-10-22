//! Construct C2, the accepted class on its acting side (decompose effects/licence.rs `volatile_free`): code run
//! ahead whose acts are extra call-local VOLATILE calls is granted whatever it may raise, and denied where an effect may
//! be read inside the same statement. Read over captured regression plans, and over the same plans with one call
//! swapped (a text mutation, as tests/c2_exact_proof.rs makes them).
use std::fs;

use gravity_decompose::corpus;
use gravity_decompose::effects::{call_local, deny_all, volatile_free, Cx, Denied, Departure, Licence};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

fn text(name: &str) -> String {
    fs::read_to_string(manifest_dir().join("tests/plans/regress-full").join(format!("{name}.plan"))).unwrap()
}

fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

/// The plan node with id `id` in the statement's tree (its plan children only).
fn node<'p>(n: Option<Node<'p>>, id: i32) -> Option<Node<'p>> {
    let n = n?;
    let p = n.as_plan()?;
    if p.plan_node_id == id {
        return Some(n);
    }
    node(p.lefttree, id).or_else(|| node(p.righttree, id))
}

/// `volatile_free` over plan `text` with `unit` the plan node of that id (`None`: the whole tree), and whether
/// `call_local` alone grants that unit.
fn read(text: &str, unit: Option<i32>) -> (Result<Licence, Denied>, bool) {
    let ctx = MemoryContext::new("c2-accepted");
    let fx = corpus::load(ctx.mcx(), text).unwrap();
    let arena = Arena::new(1 << 20);
    let cx = Cx::new(fx.stmt, &fx.facts, arena.handle());
    let u = match unit {
        Some(id) => node(fx.stmt.planTree, id).expect("the unit's node"),
        None => fx.stmt.planTree.expect("a plan"),
    };
    let r = volatile_free(&cx, [u]);
    assert_eq!(deny_all(|| volatile_free(&cx, [u])), r, "the plan-order run reads it as outside");
    (r, call_local(&cx, [u]).is_some())
}

/// limit-012 (`select unique1, unique2, nextval('testseq') from tenk1 order by tenthous limit 10`): the nextval sits in
/// the Result (node 1) above the Sort (node 2) over the scan (node 3).
const NEXTVAL: &str = "{FUNCEXPR :funcid 1574 ";

/// limit-012 with its one nextval call made a call of `funcid`.
fn swapped(funcid: u32) -> String {
    let t = text("limit-012");
    assert_eq!(t.matches(NEXTVAL).count(), 1, "one nextval call");
    t.replacen(NEXTVAL, &format!("{{FUNCEXPR :funcid {funcid} "), 1)
}

#[test]
fn a_lone_nextval_run_ahead_is_free() {
    let (r, _) = read(&text("limit-012"), None);
    let l = r.expect("nextval alone (it may raise at the sequence end: the accepted class)");
    assert!(l.accepted() && l.departure() == Departure::Ahead);
    assert!(!l.recheck() && !l.tagged() && !l.held_error() && !l.whole_read() && l.exact().is_none(), "held for the tracer only");
    assert!(read(&text("limit-012"), Some(3)).0.is_ok(), "a unit that does not act minds the nextval elsewhere");
}

#[test]
fn another_nextval_sees_only_a_gap() {
    // the Result's first column (unique1, its VAR at location 7) made a second nextval call
    let t = text("limit-012");
    let var = "{VAR :varno -2 :varattno 1 :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 1 :varattnosyn 1 :location 7}";
    assert_eq!(t.matches(var).count(), 1, "the Result's unique1");
    let call = "{FUNCEXPR :funcid 1574 :funcresulttype 20 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args ({CONST :consttype 2205 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 33 :constvalue 4 [ 155 154 0 0 0 0 0 0 ]}) :location 7}";
    let two = t.replacen(var, call, 1);
    assert_eq!(two.matches(NEXTVAL).count(), 2);
    assert!(read(&two, None).0.is_ok_and(|l| l.accepted()), "two nextval calls: each sees at most a gap");
}

#[test]
fn a_raising_acting_call_is_granted() {
    // random(int4, int4) raises at min > max
    assert!(read(&swapped(6339), None).0.is_ok_and(|l| l.accepted()));
    assert!(read(&swapped(1598), None).0.is_ok_and(|l| l.accepted()), "random()");
}

#[test]
fn a_call_that_reads_or_sets_the_moved_state_denies() {
    for (funcid, what) in [(1575, "currval"), (2559, "lastval"), (1576, "setval"), (1599, "setseed")] {
        assert_eq!(read(&swapped(funcid), None).0, Err(Denied), "{what} in the unit");
        assert_eq!(read(&swapped(funcid), Some(3)).0, Err(Denied), "{what} elsewhere in the statement");
    }
}

#[test]
fn a_user_function_anywhere_denies() {
    // a function past FIRST_NORMAL_OID the facts cannot name: user code, unknown, fail closed
    assert_eq!(read(&swapped(16999), None).0, Err(Denied), "in the unit");
    assert_eq!(read(&swapped(16999), Some(3)).0, Err(Denied), "elsewhere in the statement");
}

/// subselect-088 (`select * from (select distinct ten from tenk1) ss where ten < 10 + nextval('ts1') order by 1`): the
/// VOLATILE value decides which rows pass the filter; PostgreSQL's own plans may call it on other rows (D4146).
#[test]
fn a_volatile_value_in_a_filter_is_granted() {
    assert!(read(&text("subselect-088"), None).0.is_ok_and(|l| l.accepted()));
}

/// sequence-005 (`SELECT last_value, .. FROM foo_seq_new`): a scan of a sequence relation reads what nextval moves.
#[test]
fn a_sequence_read_denies() {
    assert_eq!(read(&text("sequence-005"), None).0, Err(Denied));
}

/// rowsecurity-102 (`WITH cte1 AS (INSERT INTO t1 VALUES (21, 'Fail') RETURNING *) SELECT * FROM cte1`).
#[test]
fn a_data_modifying_cte_denies() {
    assert_eq!(read(&text("rowsecurity-102"), None).0, Err(Denied));
}

/// copyselect-001 (`copy (select t from test1 where id=3 for update) to stdout`, a LockRows (node 0) over the scan (node
/// 1)): the scan alone is a unit call_local grants, and volatile_free denies it for the statement's row lock, as it
/// denies the whole tree.
#[test]
fn a_row_lock_anywhere_denies() {
    let (r, local) = read(&text("copyselect-001"), Some(1));
    assert!(local, "call_local grants the scan");
    assert_eq!(r, Err(Denied), "a row lock in the statement is no call's act");
    assert_eq!(read(&text("copyselect-001"), None).0, Err(Denied), "the LockRows in the unit");
}

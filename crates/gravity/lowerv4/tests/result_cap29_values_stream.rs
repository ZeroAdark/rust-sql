//! CAP-29 (result lane, lanes/result/work/cap29): past `cost::VALUES_CODED_ROWS_MAX` rows holding an expression a VALUES
//! list streams them (write.literals' stream form: one more protocol word, the execution's hook; no code per row); at or
//! under the capacity its form is unchanged; D3374: a cell reading an executor parameter streams too (its value handed
//! over at the row's turn), but only past the capacity. D2907: past `values::coded_cells_max()` cells that are not a Const (R5-B: the class
//! table's rows) the list
//! streams too; a list that cannot stream keeps the compiled form there. The answers are
//! e2e/gravity/v5/result_cap29_values_stream.sql's and result_d2907_values_cut.sql's.
#[allow(dead_code)]
use gravity_lowerv4_it as common;

use gravity_lowerv4::ops::shared::values::coded_cells_max;
use gravity_program::cost::VALUES_CODED_ROWS_MAX;
use gravity_program::protos::{literals_mark, ProtoKind};

/// select-017's one-column VALUES (`(2), (null), (1)`) with its first row repeated `rows` times, the first `coded` of
/// them `param(i)` (a PARAM node) instead of the literal.
fn values_of(rows: usize, coded: usize, param: &dyn Fn(usize) -> String) -> String {
    let text = std::fs::read_to_string(common::plans_dir().join("regress-full/select-017.plan")).expect("select-017's plan");
    let (head, rest) = text.split_once(":values_lists (").expect("select-017's VALUESSCAN lists");
    let row = &rest[..rest.find(") (").expect("its first row") + 1];
    let tail = &rest[rest.find("))}").expect("the lists' end") + 1..];
    let lists: Vec<String> = (0..rows).map(|i| if i < coded { format!("({})", param(i)) } else { row.to_string() }).collect();
    format!("{head}:values_lists ({}{tail}", lists.join(" "))
}

/// `$id` / an executor parameter `id`, int4.
fn param(kind: u8, id: usize) -> String {
    format!("{{PARAM :paramkind {kind} :paramid {id} :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 23}}")
}
/// `$1` in every coded row.
fn extern1(_: usize) -> String {
    param(0, 1)
}
/// Executor parameter 0 in every coded row.
fn exec0(_: usize) -> String {
    param(1, 0)
}

/// The Literals protocol's (word count, coded-cell mark) of the lowered list, or the refusal's text.
fn form(rows: usize, coded: usize, param: &dyn Fn(usize) -> String) -> Result<(u16, u16), String> {
    match common::lower_fixture(&values_of(rows, coded, param)) {
        Some(Ok(l)) => {
            let lit = l.descs.v4.protos.iter().find(|p| p.kind == ProtoKind::Literals).expect("the per-execution table");
            Ok((lit.words.n, (lit.arg >> 48) as u16))
        }
        Some(Err(e)) => Err(format!("{e:?}")),
        None => panic!("the {rows}-row VALUES is not decomposed"),
    }
}

#[test]
fn past_the_capacity_the_coded_rows_stream() {
    for (rows, coded) in [(VALUES_CODED_ROWS_MAX + 1, VALUES_CODED_ROWS_MAX + 1), (VALUES_CODED_ROWS_MAX + 40, VALUES_CODED_ROWS_MAX + 1)] {
        let mark = literals_mark((0..coded).map(|r| (r, 0)));
        assert_eq!(form(rows, coded, &extern1), Ok((4, mark)), "{rows} rows, {coded} of them $1: the stream form's four words");
    }
}

#[test]
fn at_or_under_the_capacity_the_form_is_unchanged() {
    // the rows past the capacity are Consts: the table's three words, the coded rows compiled (one row of code here)
    let mark = literals_mark([(0, 0)]);
    assert_eq!(form(VALUES_CODED_ROWS_MAX + 2, 1, &extern1), Ok((3, mark)), "one coded row among more than 65536");
    // at the coded cells' cost (R5-B: `values::coded_cells_max()`, the class table's rows) the coded rows stay compiled
    let n = coded_cells_max();
    assert_eq!(form(300, n, &extern1), Ok((3, literals_mark((0..n).map(|r| (r, 0))))), "300 rows, {n} coded");
}

#[test]
fn past_the_word_plans_bound_the_coded_rows_stream() {
    // 20000 rows of a distinct `$n` each: 20000 words compiled, past `cost::VALUES_CODED_WORDS_MAX` (16384)
    let rows = 20000;
    let mark = literals_mark((0..rows).map(|r| (r, 0)));
    assert_eq!(form(rows, rows, &|i| param(0, i + 1)), Ok((4, mark)), "{rows} distinct parameters: the stream form");
}

#[test]
fn an_executor_parameter_past_the_capacity_streams() {
    // D3374 (result_cap29): past the capacity the list streams and the source compiles the parameter alone, to hand its
    // value to the evaluation at each coded row's turn. This fixture's plan has no node that sets parameter 0, so that
    // compile refuses by its own name, not the capacity's (e2e values_d3374_stream_exec_param serves a LATERAL outer row's)
    let r = form(VALUES_CODED_ROWS_MAX + 1, VALUES_CODED_ROWS_MAX + 1, &exec0);
    assert!(r.as_ref().is_err_and(|e| e.contains("an executor parameter no nest, SubPlan or InitPlan of the graph sets")), "PARAM_EXEC rows past the capacity: {r:?}");
}

#[test]
fn past_the_coded_cells_cost_the_rows_stream() {
    let n = coded_cells_max() + 1;
    assert_eq!(form(n, n, &extern1), Ok((4, literals_mark((0..n).map(|r| (r, 0))))), "{n} coded cells: the stream form");
}

#[test]
fn at_the_coded_cells_cost_the_form_is_unchanged() {
    let n = coded_cells_max();
    assert_eq!(form(n + 40, n, &extern1), Ok((3, literals_mark((0..n).map(|r| (r, 0))))), "{n} coded cells: compiled");
}

#[test]
fn an_executor_parameter_past_the_cost_never_streams() {
    // D3374: under the capacity a list reading an executor parameter keeps the compiled form (`values::streams_for_cost`)
    let n = coded_cells_max() + 1;
    let r = form(n, n, &exec0);
    assert!(!matches!(r, Ok((4, _))), "{n} executor-parameter cells: not the stream form: {r:?}");
}

//! NEW-29 / CAP-11 (result lane, lanes/result/new29; group-g2, lanes/group-g2/work/n29): every VALUES list reads its
//! Const cells from write.literals' per-execution literal table at any length (no code per row, a program the executor
//! files); a row holding an expression is the program's code, its cells marked in the protocol's arg. The answers are
//! e2e/gravity/v4/result_new29_values_literal_forms.sql's and e2e/gravity/v5/group_n29_values_short_lists.sql's.
#[allow(dead_code)]
use gravity_lowerv4_it as common;

use gravity_program::protos::{literals_mark, ProtoKind};

/// select-017's one-column VALUES (`(2), (null), (1)`) with its first row repeated `rows` times, the first `params` of
/// them a `$1` instead of the literal.
fn values_of(rows: usize, params: usize) -> String {
    let text = std::fs::read_to_string(common::plans_dir().join("regress-full/select-017.plan")).expect("select-017's plan");
    let (head, rest) = text.split_once(":values_lists (").expect("select-017's VALUESSCAN lists");
    let row = &rest[..rest.find(") (").expect("its first row") + 1];
    let tail = &rest[rest.find("))}").expect("the lists' end") + 1..];
    let param = "({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 23})";
    let lists: Vec<&str> = (0..rows).map(|i| if i < params { param } else { row }).collect();
    format!("{head}:values_lists ({}{tail}", lists.join(" "))
}

/// (filed, reads the per-execution table, the coded cells' mark)
fn form(rows: usize, params: usize) -> (bool, bool, u16) {
    match common::lower_fixture(&values_of(rows, params)) {
        Some(Ok(l)) => {
            let lit = l.descs.v4.protos.iter().find(|p| p.kind == ProtoKind::Literals);
            (!l.descs.no_cache, lit.is_some(), lit.map_or(0, |p| (p.arg >> 48) as u16))
        }
        other => panic!("the {rows}-row VALUES ({params} parameters) serves: {}", other.map_or("not decomposed".into(), |r| format!("{:?}", r.err()))),
    }
}

#[test]
fn a_const_only_list_reads_the_per_execution_table_at_any_length() {
    for rows in [1, 3, 16, 256, 257, 4096] {
        assert_eq!(form(rows, 0), (true, true, 0), "{rows} literal cells: the per-execution table, a program the executor files");
    }
}

#[test]
fn a_list_holding_an_expression_reads_its_constants_from_the_table() {
    for (rows, params) in [(3, 1), (256, 1), (258, 1), (300, 2)] {
        let mark = literals_mark((0..params).map(|r| (r, 0)));
        assert_eq!(form(rows, params), (true, true, mark), "{rows} rows, {params} parameter cells: their rows coded, the rest the table");
    }
}

//! D1415 (WINDOW; W8-D1415-WINDOW-SUBPLAN-ARG, V4R-573): a SubPlan in a window function's argument. decompose declares it
//! ahead of the window in the reader pipeline (`subplan 1 nested ... site=aggarg`, SubSite::AggArg), where trunk refused
//! it by admit's name (regress window#39, `lead(ten, (SELECT two FROM tenk1 WHERE s.unique2 = unique2)) OVER (PARTITION
//! BY four ORDER BY ten)`). lead's offset is read only at the current row (PG WinGetFuncArgCurrent), so the window step now
//! evaluates it over the stored row, its SubPlan by its eager step, into that row's cell before the object reads it. A
//! SubPlan in a value argument (read at other rows) rides the partition buffer's fill (v5 subplan): this scalar one may
//! raise (a second row), so it goes on the fill under an errorhold recheck licence (held-out subq-0244), never a Bug.
use gravity_lowerv4_it as common;

fn window_026() -> String {
    let p = common::plans_dir().join("regress-full/window-026.plan");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The byte index just past the plan-text node that opens at `at` (`{`), by brace depth.
fn node_end(s: &str, at: usize) -> usize {
    assert_eq!(s.as_bytes()[at], b'{', "a node at {at}");
    let mut depth = 0usize;
    for (i, c) in s.bytes().enumerate().skip(at) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    panic!("an unclosed node at {at}");
}

/// window-026 with lead's two arguments swapped: `lead((SELECT two FROM tenk1 WHERE s.unique2 = unique2), ten)`, both
/// int4, so the SubPlan is the VALUE argument (read at the offset row, PG WinGetFuncArgInPartition).
fn value_arg_form(text: &str) -> String {
    let wf = text.find("{WINDOWFUNC ").expect("window-026 holds a WindowFunc");
    let a = wf + text[wf..].find(":args (").expect("the WindowFunc's args") + ":args (".len();
    let e1 = node_end(text, a);
    let b = e1 + 1;
    let e2 = node_end(text, b);
    assert!(text[a..e1].starts_with("{VAR ") && text[b..e2].starts_with("{SUBPLAN "), "lead(ten, (SubPlan)) in window-026");
    format!("{}{} {}{}", &text[..a], &text[b..e2], &text[a..e1], &text[e2..])
}

#[test]
fn a_window_offset_subplan_lowers() {
    let r = common::lower_fixture(&window_026()).expect("regress-full-window-026 decomposes");
    if let Err(e) = r {
        panic!("regress-full-window-026 refused: {e:?}");
    }
}

#[test]
fn a_window_value_argument_subplan_lowers() {
    let text = value_arg_form(&window_026());
    if let Err(e) = common::lower_fixture(&text).expect("the value-argument form decomposes") {
        let e = format!("{e:?}");
        assert!(!e.contains("Bug"), "a Bug refusal: {e}");
        panic!("lead((SubPlan), ten) refused: {e}");
    }
}

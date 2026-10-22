//! D627 (KEYED, main 09-25): a lowered program's IR is the same in every process. The jsonb in-place walk baked the
//! addresses of its "true" / "false" statics and the text views the address of their empty image, so under PIE + ASLR
//! m2/bench-028's IR differed on every run of one test binary (TOPS: 4 of 5,406 plan fixtures across two processes).
//! Those bytes are now written into the function's frame (step/text_like.rs `frame_bytes`); this test lowers the fixtures
//! in two child processes of this binary and compares the printed IR.
//!
//! D627-B (main 09-25, not built): the wide record tier's `RecordLayout` carries its interned `WideLayout` pointer
//! (gravity_program `intern_wide`, D069), which keyed/table_size.rs, keyed/emit_group.rs and fold/slots.rs store as a
//! constant; its fix is the per-program constant pool (a V4R after M4). Until then those fixtures stay unpinned and out of
//! this comparison, by name.
//!
//! D627-C (main 09-25), retired by NEW-29: a VALUES list holding an expression kept its literal table in the program (a
//! heap address in the IR, so `no_cache` and an owner). Every list now reads its Const cells from write.literals'
//! per-execution table (ops/values/lower.rs), so no such address is in the IR and the program is filed:
//! `a_values_list_holding_an_expression_reads_the_per_execution_table`.
#[allow(dead_code)]
use gravity_lowerv4_it as common;

/// The fixtures whose IR baked a static's address (D627 A).
const SAME_IR: [&str; 2] = ["bench-028", "bench-098"];
/// D627-B: the wide tier's interned layout pointer; out of the comparison until the constant pool lands.
const D627_B: [&str; 3] = ["regress-full-functional_deps-001", "regress-full-groupingsets-035", "tpch-sf10-001"];
const CHILD: &str = "D627_CHILD";

fn plan_text(id: &str) -> String {
    let rel = match id.strip_prefix("regress-full-") {
        Some(stem) => format!("regress-full/{stem}.plan"),
        None => format!("{id}.plan"),
    };
    std::fs::read_to_string(common::plans_dir().join(&rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn ir_of(id: &str) -> String {
    match common::lower_fixture(&plan_text(id)) {
        Some(Ok(l)) => gravity_ir::text::print_program(&l.ir),
        Some(Err(e)) => panic!("{id}: refused ({e:?}): D627's fixtures serve"),
        None => panic!("{id}: not decomposed"),
    }
}

/// One child's IR per fixture, between marker lines.
fn child() -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "d627_ir_is_the_same_in_every_process", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("a child test process");
    assert!(out.status.success(), "the child failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).expect("utf-8 IR");
    SAME_IR.iter().map(|id| {
        let (open, close) = (format!("=== d627 {id} ===\n"), format!("=== d627 end {id} ===\n"));
        let at = text.find(&open).unwrap_or_else(|| panic!("{id}: no IR in the child's output")) + open.len();
        text[at..at + text[at..].find(&close).expect("its end marker")].to_owned()
    }).collect()
}

#[test]
fn d627_ir_is_the_same_in_every_process() {
    if std::env::var(CHILD).is_ok() {
        for id in SAME_IR {
            print!("=== d627 {id} ===\n{}=== d627 end {id} ===\n", ir_of(id));
        }
        return;
    }
    let (a, b) = (child(), child());
    let bad: Vec<&str> = SAME_IR.iter().zip(a.iter().zip(&b)).filter(|(_, (x, y))| x != y).map(|(id, _)| *id).collect();
    assert!(bad.is_empty(), "IR differs between two processes of one binary (a process address baked in): {bad:?}");
    eprintln!("D627: {} fixtures' IR the same across two processes; D627-B not compared: {D627_B:?}", SAME_IR.len());
}

/// select-017's VALUES row repeated 300 times, the first row a parameter (a list holding an expression): its Const cells
/// are the per-execution table (its arg marks the coded cell), the program filed and owning no table.
#[test]
fn a_values_list_holding_an_expression_reads_the_per_execution_table() {
    let text = plan_text("regress-full-select-017");
    let (head, rest) = text.split_once(":values_lists (").expect("select-017's VALUESSCAN lists");
    let row = &rest[..rest.find(") (").expect("its first row") + 1];
    let tail = &rest[rest.find("))}").expect("the lists' end") + 1..];
    let param = "({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 23})";
    let lists = std::iter::once(param).chain(std::iter::repeat(row).take(299)).collect::<Vec<_>>().join(" ");
    let big = format!("{head}:values_lists ({lists}{tail}");
    let l = match common::lower_fixture(&big) {
        Some(Ok(l)) => l,
        other => panic!("the 300-row VALUES serves: {}", other.map_or("not decomposed".into(), |r| format!("{:?}", r.err()))),
    };
    let mark = gravity_program::protos::literals_mark([(0, 0)]);
    let lit = l.descs.v4.protos.iter().find(|p| p.kind == gravity_program::protos::ProtoKind::Literals).expect("write.literals declared");
    assert_eq!((lit.arg >> 48) as u16, mark, "the arg marks row 0's parameter cell as the program's code");
    assert!(!l.descs.no_cache, "a list holding an expression is a filed program");
}

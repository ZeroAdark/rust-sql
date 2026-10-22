//! D643 (M3-TOPS, found by M5-L0 09-25): a row top (runs.topk_rows) whose source takes its pipeline serial returned no
//! row. The top's end arms its resident run for section ends from the decomposer's descriptor (claim-driven), while
//! src.heap / src.pgrc describe the pipeline serial when a hosted call a pool thread cannot run is in it (a
//! function-manager site: `i::text`, `md5(..)`; hir.rs `pool_unsafe_site`). The top's descriptor then had no section
//! end, the driver ran one ALL claim and no `SECTION_END`, the run was never sealed and the emit's `sort_begin` found
//! none: `SELECT i, md5(i::text) FROM t ORDER BY i LIMIT 3` gave 0 rows (PG 3). The invariant pinned here, over every
//! plan fixture that lowers: a pipeline whose code cuts a resident run (`sort_cut_res*` with `sort_open_res*`) is a
//! finishing pipeline (`Pipeline::finish`), so the driver makes its section-end call; bench-073 with a restricted call
//! as a column is the fixture that fails without the fix (no plan fixture had a hosted call in a row top's pipeline).
#[allow(dead_code)]
use gravity_lowerv4_it as common;

/// bench-073's scan column `id` computed per row by a parallel-restricted builtin: `SELECT pg_stat_get_backend_pid(a), t
/// FROM g WHERE a > 20 ORDER BY b LIMIT 1000`, a hosted site the pool cannot run (proparallel 'r'; hir.rs
/// `pool_unsafe_site`), as i::text or md5 are on the server (function-manager sites; the rig's catalog has no collation
/// seam for their text).
const T: &str = "{VAR :varno 1 :varattno 1 :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 1 :varattnosyn 1 :location 27}";
const A_IO: &str = "{FUNCEXPR :funcid 1937 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args ({VAR :varno 1 :varattno 2 :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 1 :varattnosyn 2 :location 51}) :location 27}";

/// The plan fixtures (the decompose corpus and its regress-full directory) whose text has a Limit over a Sort, and
/// D643's variant of bench-073 (a hosted output column).
fn tops() -> Vec<(String, String)> {
    let dir = common::plans_dir();
    let all = [common::files(&dir, "plan"), common::files(&dir.join("regress-full"), "plan")].concat();
    let mut v: Vec<(String, String)> = all.into_iter().filter(|(_, t)| t.contains("{LIMIT") && t.contains("{SORT")).collect();
    let b = std::fs::read_to_string(dir.join("bench-073.plan")).expect("bench-073.plan");
    assert_eq!(b.matches(T).count(), 1, "bench-073's scan column id, once");
    v.push(("d643-bench-073-restricted".to_owned(), b.replace(T, A_IO)));
    v
}

#[test]
fn d643_a_resident_top_run_has_its_section_end() {
    let (mut bad, mut seen) = (Vec::new(), 0);
    for (name, text) in tops() {
        let r = common::lower_fixture(&text);
        if name.starts_with("d643-") {
            if let Some(Err(e)) = &r {
                panic!("{name}: D643's variant lowers ({e:?})");
            }
            assert!(r.is_some(), "{name}: D643's variant decomposes");
        }
        let Some(Ok(l)) = r else { continue };
        let ir = gravity_ir::text::print_program(&l.ir);
        if name.starts_with("d643-") {
            assert!(ir.contains("call_host sort_cut_res") && l.descs.pipelines.iter().any(|p| !p.is_eligible()), "{name}: a row top over a pipeline its hosted call keeps serial");
        }
        for (k, p) in l.descs.pipelines.iter().enumerate() {
            let Some(f) = fn_text(&ir, k) else { continue };
            if f.contains("call_host sort_cut_res") && f.contains("call_host sort_open_res") {
                seen += 1;
                if !p.finish {
                    bad.push(format!("{name}: p{k} cuts a resident run but has no section end ({:?})", p.parallel));
                }
            }
        }
    }
    assert!(seen > 0, "no fixture lowers a resident row top");
    assert!(bad.is_empty(), "{} resident top(s) without a section end:\n{}", bad.len(), bad.join("\n"));
}

/// Pipeline `k`'s function text in the printed program: the `k`-th function. The program declares one function per
/// pipeline in the descriptors' order (frame/program.rs `functions`), each named after its graph pipeline, so a pair
/// pipeline `P'` the lowering adds (spill_pair.rs) is named past the graph's last one and the names after it no longer
/// match their descriptor's index.
fn fn_text(ir: &str, k: usize) -> Option<&str> {
    let starts: Vec<usize> = ir.match_indices("\n  (fn p").map(|(i, _)| i + 1).collect();
    let at = *starts.get(k)?;
    Some(&ir[at..starts.get(k + 1).copied().unwrap_or(ir.len())])
}

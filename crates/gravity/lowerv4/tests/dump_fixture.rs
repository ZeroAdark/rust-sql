//! The gate's IR dump for lowerv4 (v5 serial step, landing 1): `scripts/gravity-gate.sh`'s `IR_DUMP_TEST` has been empty
//! since lowerv2's `dump_fixture` was removed with it on 2026-09-23 (tag lowerv2-final), so every gate since then ran without
//! `ir.txt`: no `--scope ir-diff`, no rowpath audit, no IR evidence for gravity-land.sh's claim flips. This is its lowerv4 form.
//! `DUMP_ALL_IR=<out file> cargo test -p gravity_lowerv4 --test dump_fixture`: every decompose plan fixture (the rigs'
//! corpus: the top level and regress-full, the same corpus and block names as lowerv2's dump, so fixture-map.tsv and
//! ir-scope.py read it unchanged) decomposed with the server's member set and lowered by v4 as the admission does; one block
//! per fixture, `=== <fixture>`, then `decompose: ..` / `refused: ..` / `lowered` with the run-time descriptors (layout,
//! sites, out, write, pipelines, the v4 protocols: rerun sets and members, locks) and the IR text. Descriptor code and heap
//! addresses print as `0x…` (they move per process); ir-scope.py masks the IR's baked heap addresses (D201). The first line
//! is the provenance header ir-provenance.py checks. Without DUMP_ALL_IR the test returns at once.
#[allow(unused_imports, dead_code)]
use gravity_lowerv4_it as common;

#[allow(dead_code)]
#[path = "common/write_rig.rs"]
mod write_rig;

use std::io::Write;

#[test]
fn dump_all() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let Ok(path) = std::env::var("DUMP_ALL_IR") else { return };
    let mut all = common::files(&common::plans_dir(), "plan");
    all.extend(common::files(&common::plans_dir().join("regress-full"), "plan").into_iter().map(|(n, t)| (format!("regress-full/{n}"), t)));
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap_or_else(|e| panic!("{path}: {e}")));
    writeln!(out, "{}", provenance()).unwrap();
    let (mut lowered, mut refused, mut declined, mut panicked) = (0usize, 0usize, 0usize, 0usize);
    for (name, text) in all {
        let block = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_rig::declared(&text, |a, graph, facts| {
                gravity_expr::nodes::func::acl_defer_begin();
                let l = gravity_lowerv4::lower_graph(a.handle(), rig.mcx(), graph, facts, common::RigCatalog::get(), &mut gravity_lower::Sequential::new(a.handle()), gravity_lower::LowerOpts::default());
                gravity_expr::nodes::func::acl_defer_end();
                // D3437: DUMP_ARM_ORDER=1 adds the program order of the graph's pipelines where arm runs move it (records only)
                let order = std::env::var_os("DUMP_ARM_ORDER").map_or_else(String::new, |_| {
                    let o = gravity_lowerv4::program_order(a.handle(), rig.mcx(), graph, facts, common::RigCatalog::get());
                    if o.iter().enumerate().all(|(i, &k)| i == k) { String::new() } else { format!("order {o:?}\n") }
                });
                match l {
                    Err(e) => format!("refused: {e}\n{order}"),
                    Ok(l) => format!(
                        "lowered\n{order}layout {:?}\nsites {:?}\nout {:?}\nwrite {:?}\npipelines {:?}\nout_region {}\nno_cache {}\nprotos {:?}\nlocks {:?}\nfuncs {:?}\n{}",
                        l.descs.layout, l.descs.sites, l.descs.out, l.descs.write, l.descs.pipelines, l.descs.out_region, l.descs.no_cache, l.descs.v4.protos, l.descs.locks, l.funcs,
                        gravity_ir::text::print_program(&l.ir)
                    ),
                }
            })
            .unwrap_or_else(|| "decompose: declined\n".to_owned())
        }))
        .unwrap_or_else(|_| "panicked\n".to_owned());
        match block.split('\n').next().unwrap_or("") {
            "lowered" => lowered += 1,
            "panicked" => panicked += 1,
            l if l.starts_with("decompose") => declined += 1,
            _ => refused += 1,
        }
        write!(out, "=== {name}\n{}", no_addrs(&block)).unwrap();
    }
    out.flush().unwrap();
    eprintln!("dump_all: {path}: lowered {lowered}, refused {refused}, decompose declined {declined}, panicked {panicked}");
}

/// The dump's first line: `# ir-dump sha=<HEAD> ctree=<scripts/gravity-gate/ctree.py hash> dirty=<0|1>` (lowerv2's form):
/// the tree it was dumped from, checked by gravity-land.sh / gravity-gate.sh (ir-provenance.py) before it becomes a baseline.
fn provenance() -> String {
    // D793-b: the checkout under test, read at run time, never env!()'s compile-time path
    let manifest: std::path::PathBuf = std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)");
    let repo = manifest.join("../../..");
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd).args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let r = repo.to_string_lossy().into_owned();
    let sha = run("git", &["-C", &r, "rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let dirty = run("git", &["-C", &r, "status", "--porcelain", "-uno", "--", "crates", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo"]).map(|s| !s.is_empty()).unwrap_or(true);
    let ctree_py = repo.join("scripts/gravity-gate/ctree.py").to_string_lossy().into_owned();
    let ctree = run("python3", &[&ctree_py, "hash", &r, "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    format!("# ir-dump sha={sha} ctree={ctree} dirty={}", u8::from(dirty))
}

/// Every `0x<hex>` (a descriptor's function pointer, a raw pointer's Debug) as `0x…`: it moves per process and per binary.
fn no_addrs(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'0' && i + 2 < b.len() && b[i + 1] == b'x' && b[i + 2].is_ascii_hexdigit() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            out.push_str("0x…");
            i += 2;
            while i < b.len() && b[i].is_ascii_hexdigit() {
                i += 1;
            }
        } else {
            let c = text[i..].chars().next().unwrap_or('?');
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

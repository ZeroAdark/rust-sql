//! The program dump knob (the user 09-24): `pgrust.gravity_dump_dir` (superuser; env seed `PGRUST_GRAVITY_DUMP_IR`) names a
//! directory every compiled Gravity program is written to (`gravity_backend::dump`: `.ir`, `.<arch>.bin`, `.meta`);
//! `scripts/gravity-dump-asm.sh` disassembles a dump. Read on a program-cache miss only.

use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob { name: "pgrust.gravity_dump_dir", kind: Kind::Builtin, default: "empty (off)", doc: "superuser: every compiled program's IR, machine code and meta written to this directory (gravity_backend::dump)" },
    Knob { name: "PGRUST_GRAVITY_DUMP_IR", kind: Kind::Env, default: "unset", doc: "the startup value of pgrust.gravity_dump_dir (guc store.rs)" },
];

fn dir() -> Option<String> {
    let v = &guc_tables::vars::pgrust_gravity_dump_dir;
    if !v.installed() {
        return None;
    }
    v.read()
}

fn meta() -> String {
    elog::with_debug_query_string(|q| {
        // one line: the statement's newlines and tabs as spaces
        let q = q.unwrap_or("unknown").replace(['\n', '\r', '\t'], " "); // alloc: Cold
        let a = &guc_tables::vars::application_name;
        let app = if a.installed() { a.read().unwrap_or_default() } else { String::new() }.replace(['\n', '\r', '\t'], " "); // alloc: Cold
        format!("app {app}\nstatement {q}\n") // alloc: Cold
    })
}

/// Registers the dump's hooks with the code cache (once per process).
pub(crate) fn install_dump_hooks() {
    gravity_backend::dump::install(gravity_backend::dump::Hooks { dir, meta });
}

/// v5 identity (day 0, landing 1): one install's record (`gravity_backend::dump::write_prog`): each program of the composed
/// lowering (main, InitPlans, arms) with its run-time descriptors: what the runner reads besides the code (layout, sites, out
/// columns, the write plan, the pipelines, the v4 protocols: rerun sets and their members, spools, inits; the locks, the
/// instrumented cells, the hosted owner's signature), then the statement's word template and recipe count. Cold: only with
/// `pgrust.gravity_dump_dir` set.
/// D3182: `pipelines` are the ASSEMBLED programs' (`finals`: main, then each InitPlan, then each arm), so exec's own verdicts
/// (`serial_over_index_sites`, `serial_over_temp_sources`) show; the lowerer's descriptors follow as `lowered_pipelines`
/// only where exec changed one.
pub(crate) fn write_install(dir: &str, c: &gravity_lower::Composed, finals: &[&[gravity_program::Pipeline]], words: &[crate::admit::WordTpl], recipes: usize) {
    let descs = |l: &gravity_lower::Lowered, fin: Option<&[gravity_program::Pipeline]>, extra: String| {
        let owner = l.descs.owner.take();
        let sig = owner.as_ref().map(|(_, s)| s.clone()); // alloc: Cold
        l.descs.owner.set(owner);
        let lowered: &[gravity_program::Pipeline] = &l.descs.pipelines;
        let pipes = fin.unwrap_or(lowered);
        let changed = if pipes == lowered { String::new() } else { format!("lowered_pipelines {lowered:?}\n") }; // alloc: Cold
        format!( // alloc: Cold
            "layout {:?}\nsites {:?}\nout {:?}\nwrite {:?}\npipelines {:?}\n{changed}out_region {}\nno_cache {}\ninstr {:?}\nprotos {:?}\nused {}\nlocks {:?}\nfuncs {:?}\nowner_sig {:?}\n{extra}",
            l.descs.layout, l.descs.sites, l.descs.out, l.descs.write, pipes, l.descs.out_region, l.descs.no_cache, l.descs.instr, l.descs.v4.protos, l.descs.used, l.descs.locks, l.funcs, sig
        )
    };
    let mut fin = finals.iter().copied();
    let mut parts = Vec::new(); // alloc: Cold
    parts.push((String::from("main"), &c.main.ir, descs(&c.main, fin.next(), format!("words {words:?}\nrecipes {recipes}\nmerge {:?}\n", c.merge)))); // alloc: Cold
    for (k, i) in c.inits.iter().enumerate() {
        parts.push((format!("init {k} param {}", i.param), &i.lowered.ir, descs(&i.lowered, fin.next(), String::new()))); // alloc: Cold
    }
    for (k, a) in c.arms.iter().enumerate() {
        parts.push((format!("arm {k}"), &a.ir, descs(a, fin.next(), String::new()))); // alloc: Cold
    }
    gravity_backend::dump::write_prog(dir, &parts);
}

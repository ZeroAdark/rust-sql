//! The JIT perf map knob: `pgrust.gravity_jit_perfmap` (superuser; env seed `PGRUST_GRAVITY_JIT_PERFMAP`) = off | func |
//! tags names the machine code the code cache compiles in `/tmp/perf-<pid>.map` (`gravity_backend::perfmap`). Read at
//! each lowering and each compile; off by default.

use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob { name: "pgrust.gravity_jit_perfmap", kind: Kind::Builtin, default: "off", doc: "superuser: off | func | tags; the JIT perf map /tmp/perf-<pid>.map of every program compiled while on (gravity_backend::perfmap)" },
    Knob { name: "PGRUST_GRAVITY_JIT_PERFMAP", kind: Kind::Env, default: "unset", doc: "the startup value of pgrust.gravity_jit_perfmap (guc store.rs; 1 / on = func)" },
];

fn mode() -> u8 {
    let v = &guc_tables::vars::pgrust_gravity_jit_perfmap;
    if !v.installed() {
        return gravity_backend::perfmap::OFF;
    }
    match v.read() {
        guc_tables::consts::GRAVITY_JIT_PERFMAP_FUNC => gravity_backend::perfmap::FUNC,
        guc_tables::consts::GRAVITY_JIT_PERFMAP_TAGS => gravity_backend::perfmap::TAGS,
        _ => gravity_backend::perfmap::OFF,
    }
}

fn statement() -> String {
    elog::with_debug_query_string(|q| q.unwrap_or("unknown").replace(['\n', '\r', '\t'], " ")) // alloc: Cold
}

/// Registers the map's hooks with the code cache (once per process).
pub(crate) fn install_perfmap_hooks() {
    gravity_backend::perfmap::install(gravity_backend::perfmap::Hooks { mode, statement });
}

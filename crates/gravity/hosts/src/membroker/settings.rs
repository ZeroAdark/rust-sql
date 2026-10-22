//! S1 criterion 11 (everything visible in SQL): `pgrust_memory_settings()`, the memory policy's knobs and the memory
//! switches the environment sets, each with its value now, its default and where the value came from. The rows come from
//! the knobs' one table (`mempolicy::consts::each`: a new knob is a new row with no change here) and from the switches'
//! own readers (`SWITCHES`: the value the code reads, never the environment parsed again). scripts/memory-broker-views.sql
//! adds the memory GUCs from pg_settings in its view `pgrust_memory_settings`. Always on: reading it changes nothing.

use datum::Datum;
use types_error::PgResult;
use types_fmgr::{varlena_result, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

/// The row's columns (scripts/memory-broker-views.sql declares the same, in order): kind, name, value, default_value,
/// source, set_by, doc.
pub const SETTINGS_COLS: usize = 7;

/// A value as the view shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Val {
    N(u64),
    S(&'static str),
}

/// One row: kind 'knob' (a `PGRUST_MEMPOLICY` key) or 'env' (a switch its own variable sets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: &'static str,
    pub name: &'static str,
    pub value: Val,
    pub default: Val,
    /// default, PGRUST_MEMPOLICY, environment (its variable is set) or set (by the process: a test, a measurement).
    pub source: &'static str,
    /// What sets it: `PGRUST_MEMPOLICY` or the switch's variable.
    pub set_by: &'static str,
    pub doc: &'static str,
}

/// A memory switch read from its own environment variable, by the reader the code uses.
struct Switch {
    name: &'static str,
    var: &'static str,
    value: fn() -> Val,
    default: fn() -> Val,
    doc: &'static str,
}

fn on(b: bool) -> Val {
    Val::N(u64::from(b))
}

/// Hearth (the page pool's `PoolAlloc`) or mimalloc serves this process's allocations.
fn allocator(hearth: bool) -> Val {
    Val::S(if hearth { "hearth" } else { "mimalloc" })
}

/// The pool's first two tenants (mcx, Gravity) are on unless `=0` where Hearth serves (a pool-global build); every other
/// tenant, and every tenant under mimalloc, is off unless `=1`.
fn tenant_default() -> Val {
    on(pagepool::pool_allocates())
}

const SWITCHES: [Switch; 17] = [
    Switch {
        name: "depot_fetch",
        var: "PGRUST_GRAVITY_DEPOT_FETCH",
        value: || on(gravity_driver::slots::depot_fetch()),
        default: || Val::N(1),
        doc: "D4709: a statement with by-rowid fetch sites parks its worker slots in the depot with their fetch recipes, so its next run takes the set; =0 turns it off",
    },
    Switch {
        name: "cache_purge_now",
        var: "PGRUST_CACHE_PURGE_NOW",
        value: || on(memruntime::purge::enabled()),
        default: || Val::N(0),
        doc: "cache plan C3 (purge (a)): the purger gives cache memory back to the OS at once; =1 turns it on",
    },
    Switch {
        name: "query_purge_bg",
        var: "PGRUST_QUERY_PURGE_BG",
        value: || on(memruntime::purge::query_bg()),
        default: || Val::N(0),
        doc: "cache plan C4 (purge (a)): query arenas purge in the background, on the purger's ticks; =1 turns it on",
    },
    Switch {
        name: "purge_pressure_mode",
        var: "PGRUST_PURGE_PRESSURE_MODE",
        value: || on(memruntime::purge::pressure_mode()),
        default: || Val::N(0),
        doc: "the purger's pressure mode: a ladder pass that left a need, or an R6 let-go, puts the purger in it; =1 turns it on",
    },
    Switch {
        name: "purge_after_idle_ms",
        var: "PGRUST_PURGE_AFTER_IDLE_S",
        value: || match memruntime::purge::hold_ms() {
            u64::MAX => Val::S("never"),
            ms => Val::N(ms),
        },
        default: || Val::N(0),
        doc: "the purger's hold: freed pages stay until this long idle (ms; the variable says whole seconds above 1, or never); 0 = no hold",
    },
    Switch {
        name: "reuse_before_release",
        var: "PGRUST_REUSE_BEFORE_RELEASE",
        value: || on(allocarena::reuse_on()),
        default: || Val::N(0),
        doc: "general heaps take the cache arena's committed free slices before fresh ones (mimalloc's arenas; 0 where no cache arena serves); =1 turns it on",
    },
    Switch {
        name: "purge_pressure_only",
        var: "PGRUST_PURGE_PRESSURE_ONLY",
        value: || on(mcx::pressure_only()),
        default: || Val::N(0),
        doc: "purge (d): mimalloc purges on no timer or collect, only under ladder pressure; =1 turns it on",
    },
    Switch {
        name: "purge_pressure_bg",
        var: "PGRUST_PURGE_PRESSURE_BG",
        value: || on(mcx::purge_bg()),
        default: || Val::N(0),
        doc: "purge (d): a release that meets ladder pressure wakes the purger, which trims the parked regions and purges; =1 turns it on",
    },
    Switch {
        name: "idle_trim_pressure_only",
        var: "PGRUST_IDLE_TRIM_PRESSURE_ONLY",
        value: || on(mcx::idle_trim_pressure_only()),
        default: || Val::N(0),
        doc: "the idle release keeps the parked regions and slot sets unless ladder pressure holds; =1 turns it on",
    },
    Switch {
        name: "make_room_log",
        var: "PGRUST_MAKE_ROOM_LOG",
        value: || on(gravity_driver::env::make_room_log()),
        default: || Val::N(0),
        doc: "D4690 (a debug lever): one log line per inline make_room; =1 turns it on",
    },
    Switch {
        name: "allocator",
        var: "PGRUST_ALLOCATOR",
        value: || allocator(pagepool::pool_allocates()),
        default: || allocator(pagepool::GLOBAL_BUILD && pagepool::hearth_default()),
        doc: "the global allocator: hearth (the page pool) or mimalloc, chosen at the first allocation; the server's default is hearth on Linux aarch64 and x86_64 and mimalloc elsewhere",
    },
    Switch {
        name: "pool_mcx",
        var: "PGRUST_POOL_MCX",
        value: || on(pagepool::switches().mcx),
        default: tenant_default,
        doc: "Hearth: memory contexts draw their blocks from the pool (on unless =0 where Hearth serves)",
    },
    Switch {
        name: "pool_gravity",
        var: "PGRUST_POOL_GRAVITY",
        value: || on(pagepool::switches().gravity),
        default: tenant_default,
        doc: "Hearth: Gravity's window segments and build buffers come from the pool (on unless =0 where Hearth serves)",
    },
    Switch {
        name: "pool_cache",
        var: "PGRUST_POOL_CACHE",
        value: || on(pagepool::switches().cache),
        default: || Val::N(0),
        doc: "Hearth: the cache tenant's switch (any tenant on starts the pool at the allocator's start); =1 turns it on",
    },
    Switch {
        name: "pool_buffers",
        var: "PGRUST_POOL_BUFFERS",
        value: || on(pagepool::switches().buffers),
        default: || Val::N(0),
        doc: "Hearth: the buffers tenant's switch (any tenant on starts the pool at the allocator's start); =1 turns it on",
    },
    Switch {
        name: "pool_keepers",
        var: "PGRUST_POOL_KEEPERS",
        value: || on(pagepool::switches().keepers),
        default: || Val::N(0),
        doc: "Hearth: mcx's keeper blocks come from the pool too (unset: parked as before); =1 turns it on",
    },
    Switch {
        name: "pool_debug",
        var: "PGRUST_POOL_DEBUG",
        value: || on(pagepool::switches().debug),
        default: || Val::N(0),
        doc: "Hearth: the pool's debug arm (a debug lever); =1 turns it on",
    },
];

/// Every row: the knobs in `KNOBS`' order, `trace`, then the switches.
pub fn rows(out: &mut dyn FnMut(Row)) {
    mempolicy::consts::each(|k, value, source| {
        out(Row { kind: "knob", name: k.name, value: Val::N(value), default: Val::N(k.default), source: source.name(), set_by: "PGRUST_MEMPOLICY", doc: k.doc })
    });
    let trace = mempolicy::trace::on();
    out(Row {
        kind: "knob",
        name: "trace",
        value: on(trace),
        default: Val::N(0),
        source: if trace { mempolicy::consts::Source::Env.name() } else { mempolicy::consts::Source::Default.name() },
        set_by: "PGRUST_MEMPOLICY",
        doc: "one log record per memory policy decision and statement end",
    });
    for s in &SWITCHES {
        let source = if s.name == "depot_fetch" && gravity_driver::slots::depot_fetch_was_set() {
            "set"
        } else if gravity_driver::env::set(s.var) {
            "environment"
        } else {
            "default"
        };
        out(Row { kind: "env", name: s.name, value: (s.value)(), default: (s.default)(), source, set_by: s.var, doc: s.doc });
    }
}

/// A text value written straight into the result context.
fn text<'m>(mcx: mcx::Mcx<'m>, s: &[u8]) -> Datum {
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(s);
    varlena_result(datum::Varlena::from_image(img))
}

/// A number's decimal digits, no allocation.
fn digits(mut n: u64, b: &mut [u8; 20]) -> &[u8] {
    let mut i = b.len();
    loop {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &b[i..];
        }
    }
}

fn val<'m>(mcx: mcx::Mcx<'m>, v: Val) -> Datum {
    match v {
        Val::N(n) => text(mcx, digits(n, &mut [0; 20])),
        Val::S(s) => text(mcx, s.as_bytes()),
    }
}

/// `pgrust_memory_settings()`: kind ('knob': a `PGRUST_MEMPOLICY` key, every one of them; 'env': a memory switch its own
/// variable sets), name, value (now, as the code reads it), default_value, source (default, PGRUST_MEMPOLICY, environment,
/// set), set_by (`PGRUST_MEMPOLICY` or the variable) and doc.
pub fn fc_pgrust_memory_settings(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pgrust_memory_settings: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    if srf.tupdesc.natts as usize != SETTINGS_COLS {
        return Err(Box::new(types_error::PgError::error("the memory views' installed declaration is another build's: run scripts/memory-broker-views.sql again").with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH))); // alloc: Cold
    }
    let mut failed = None;
    rows(&mut |r| {
        if failed.is_some() {
            return;
        }
        let values = [
            text(mcx, r.kind.as_bytes()),
            text(mcx, r.name.as_bytes()),
            val(mcx, r.value),
            val(mcx, r.default),
            text(mcx, r.source.as_bytes()),
            text(mcx, r.set_by.as_bytes()),
            text(mcx, r.doc.as_bytes()),
        ];
        if let Err(e) = srf.putvalues(&values, &[false; SETTINGS_COLS]) {
            failed = Some(e);
        }
    });
    match failed {
        Some(e) => Err(e),
        None => Ok(srf.finish(fcinfo)),
    }
}

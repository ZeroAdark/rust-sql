//! The memory roots of the crates below the driver (rule C14: a root memory context is created only in mem). The driver's
//! roots are made in driver/src/mem/root.rs; gravity_expr cannot name the driver, so the one-off roots its compile and
//! its cold error paths read the catalog through (C `CurrentMemoryContext` scratch for a syscache lookup, a cached
//! tuple descriptor's or json cache's own context) come from here.
use mcx::MemoryContext;

/// A root context `name` (mcx `MemoryContext::new`), handed by value to the owner that keeps it: dropped with a local
/// lookup's scratch, or boxed beside a cache whose handles must keep its address.
pub fn root_context(name: &'static str) -> MemoryContext {
    MemoryContext::new(name)
}

/// D2983: a tier C (function-manager) builtin a pool thread may run, `proparallel = 's'` aside (the caller's
/// check): its body reads no catalog and no session state beyond what a claim carries (the session's GUCs, hosts
/// `SessionCarry`; the `StatementArm`'s encoding / zone / collations), and its `fn_extra` is per thread (each pool lane
/// twins the call).
/// Covered: the pure input functions of the soft-error row and the transaction / statement timestamps. Every other tier C
/// row stays on the leader (the catalog readers, the privilege checks, current_setting, the SRF / window / record rows).
pub fn pool_hosted(oid: u32) -> bool {
    let Some(r) = crate::row_of(oid) else { return false };
    // a shim row bound through the function manager (CoerceViaIO's input half binds `textin` with its three I/O
    // arguments, the shim's arity is one): the same body a shim site runs on a pool thread
    if matches!(r.tier, crate::Tier::A | crate::Tier::B) {
        return !r.stub;
    }
    if r.reason == "context: soft-error context" {
        return POOL_INPUTS.contains(&r.name);
    }
    r.reason.starts_with("context: session/transaction state (") && POOL_SESSION.contains(&r.name)
}

const POOL_INPUTS: &[&str] = &[
    "int2in", "int4in", "int8in", "float4in", "float8in", "boolin", "uuid_in", "date_in", "time_in", "timetz_in", "timestamp_in",
    "timestamptz_in", "interval_in", "bpcharin", "varcharin", "byteain", "inet_in", "cidr_in", "macaddr_in", "macaddr8_in", "pg_lsn_in",
    "bit_in", "varbit_in", "json_in", "jsonb_in", "xidin", "xid8in", "cidin", "tidin", "oidin", "point_in", "lseg_in", "path_in", "box_in",
    "poly_in", "circle_in", "line_in",
];

const POOL_SESSION: &[&str] = &["now", "transaction_timestamp", "statement_timestamp"];

/// B2: the claim generation. Each carry captured on a leader (`SessionCarry::capture`, once per section submit) draws a
/// new one; a claim arms it on its thread for its life (`arm_claim_gen`, put back when the claim ends). A pool lane
/// entered under another generation than it last ran under drops its twins' `fn_extra` (lowerv4 hir.rs `refresh_extras`):
/// a memo may point into the previous statement's armed catalog images. 0 = no claim armed (the leader's own thread).
static NEXT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A new claim generation (on the leader, at capture).
pub fn new_claim_gen() -> u64 {
    NEXT_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// This thread's armed claim generation (0 when no claim is armed).
pub fn claim_gen() -> u64 {
    crate::typesem::collation::CLAIM_GEN.with(|g| g.get())
}

/// Arm claim generation `g` on this thread; the previous one, for the claim's end to put back.
pub fn arm_claim_gen(g: u64) -> u64 {
    crate::typesem::collation::CLAIM_GEN.with(|c| c.replace(g))
}


//! D537: fmgr_info's non-builtin leg for the rig. gravity_expr binds a hosted call's FmgrInfo at lowering (nodes/func.rs
//! `bind` -> gravity_fn `FmgrCall::bind` -> `fmgr_info`); a function outside fmgr's builtin table (a LANGUAGE sql one:
//! obj_description) reads pg_proc through `syscache_seams::lookup_pg_proc_fmgr` and takes the registered SQL handler. The
//! server installs both (seams_init, sql_functions); the rig answers the seam from pg_proc.dat and registers a SQL handler
//! that panics if a program ever calls it (the rig runs no host site).
use std::collections::HashMap;

use syscache_seams::PgProcFmgrShape;
use types_core::Oid;

fn shapes() -> &'static HashMap<Oid, PgProcFmgrShape> {
    static SHAPES: std::sync::OnceLock<HashMap<Oid, PgProcFmgrShape>> = std::sync::OnceLock::new();
    SHAPES.get_or_init(|| {
        let types: HashMap<String, Oid> = super::dat("pg_type.dat").into_iter().filter_map(|e| Some((e.get("typname")?.clone(), e.get("oid")?.parse().ok()?))).collect();
        super::dat("pg_proc.dat").into_iter().filter_map(|e| {
            let prolang = match e.get("prolang").map(String::as_str) { Some("sql") => fmgr_core::SQL_LANGUAGE_ID, Some("c") => fmgr_core::C_LANGUAGE_ID, _ => fmgr_core::INTERNAL_LANGUAGE_ID };
            let t = |k: &str| e.get(k).is_some_and(|v| v == "t");
            let shape = PgProcFmgrShape {
                prolang,
                prorettype: e.get("prorettype").and_then(|n| types.get(n)).copied().unwrap_or(0),
                pronargs: e.get("proargtypes").map_or(0, |a| a.split_whitespace().count() as i16),
                proisstrict: e.get("proisstrict").is_none_or(|v| v == "t"),
                proretset: t("proretset"),
                prosecdef: t("prosecdef"),
                proconfig_isnull: true,
                xmin: types_core::FrozenTransactionId,
                tid: Default::default(),
            };
            Some((e.get("oid")?.parse().ok()?, shape))
        }).collect()
    })
}

pub(super) fn install() {
    syscache_seams::lookup_pg_proc_fmgr::set(|f| Ok(shapes().get(&f).copied()));
    fmgr_core::register_sql_language_handler(|_, _| panic!("v4 rig: a SQL-language function called (the rig runs no host site)"));
}

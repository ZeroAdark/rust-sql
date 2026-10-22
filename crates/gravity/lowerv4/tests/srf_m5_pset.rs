//! M5-PSET (cards/M5.md `srf/project_set.rs`, `srf/limit_cut.rs`; M5-plan §3.3, §3.8): kind srf.projectset over the srf
//! goldens (decompose tests/plans/srf.sql), run by the crate's driver in every mode: straight, a stop after every row
//! (and every second and third: a stop lands mid-set and the row re-enters at its offset word), cancel, scribble. The
//! rows are PG 18.6's (checked on box 53, psql). G4: a plain Project is not this kind.
use gravity_lowerv4_it as common;

use common::multiset::Outcome;
use common::{run_fixture, Mode, MODES};

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `name`'s rows in every mode equal `want` (a cancel either ends with PG's cancel error or finishes first).
fn rows_in_every_mode(name: &str, want: &[&[Option<&str>]]) {
    let want: Vec<Vec<Option<String>>> = want.iter().map(|r| r.iter().map(|c| c.map(str::to_owned)).collect()).collect();
    let t = text(&format!("{name}.plan"));
    for mode in MODES.into_iter().chain([Mode::Stop(2), Mode::Stop(3)]) {
        let got = run_fixture(&t, mode).unwrap_or_else(|| panic!("{name}: the fixture does not decompose"));
        match (mode, got) {
            (_, Ok(Outcome::Rows(r))) => assert_eq!(r, want, "{name} {}", common::mode_name(mode)),
            (Mode::Cancel(_), Ok(Outcome::Error { sqlstate, .. })) => assert_eq!(sqlstate, "57014", "{name}: cancel"),
            (_, other) => panic!("{name} {}: {other:?}", common::mode_name(mode)),
        }
    }
}

/// srf-002 `SELECT generate_series(1, 3)`: one set over the Result's one row.
#[test]
fn one_series() {
    rows_in_every_mode("srf-002", &[&[Some("1")], &[Some("2")], &[Some("3")]]);
}

/// srf-003 `SELECT generate_series(1, 3), generate_series(1, 2)`: lockstep, the shorter set NULL-padded (M5 L0-c (1)).
#[test]
fn two_series_in_lockstep() {
    rows_in_every_mode("srf-003", &[&[Some("1"), Some("1")], &[Some("2"), Some("2")], &[Some("3"), None]]);
}

/// srf-013 `SELECT generate_series(1, 5) OFFSET 2`: the OFFSET rows are produced and skipped after the set loop.
#[test]
fn offset_rows_are_produced_then_skipped() {
    rows_in_every_mode("srf-013", &[&[Some("3")], &[Some("4")], &[Some("5")]]);
}

/// Every target-list golden is kind srf.projectset (what admission sees; the LIMIT goldens' srf.limitcut is required at
/// the step's lowering, which srf-013 above runs); G4: srf-009.1 (a SQL function the planner inlined into a Result: a
/// plain Project) is not this kind.
#[test]
fn projectset_goldens_take_the_kind_and_a_plain_project_does_not() {
    for name in ["srf-002", "srf-003", "srf-004", "srf-005", "srf-012", "srf-013"] {
        let (kinds, _) = common::props_fixture(&text(&format!("{name}.plan"))).unwrap_or_else(|| panic!("{name}: no props"));
        assert!(kinds.contains("srf.projectset"), "{name}: {kinds:?}");
    }
    let (kinds, _) = common::props_fixture(&text("srf-009.1.plan")).expect("srf-009.1 props");
    assert!(!kinds.contains("srf.projectset"), "srf-009.1: {kinds:?}");
}

/// Host sets lower through one SetFn protocol per step (srf/cell.rs): srf-011 `jsonb_array_elements` (a library entry) and
/// srf-008's `unnest` beside an iterator (the array face). The crate runs no protocol, so the server runner judges their
/// rows (e2e m5_lockstep). A SQL set function in the target list (srf-009 `three()`) is the next test's (it needs the
/// pg_proc seam).
#[test]
fn host_sets_declare_their_setfn_protocol() {
    for name in ["srf-011", "srf-008"] {
        let l = common::lower_fixture(&text(&format!("{name}.plan"))).expect("decomposes").unwrap_or_else(|r| panic!("{name}: {r}"));
        let n = l.descs.v4.protos.iter().filter(|p| p.kind == gravity_program::protos::ProtoKind::SetFn).count();
        assert_eq!(n, 1, "{name}: one SetFn protocol");
    }
}

/// M5 Decision 7 (V4R-258): a function-manager set's SetFn protocol carries `SETFN_PRIVATE` (its program stays the
/// session's, exec admit.rs `shareable`) whether or not it names a node; a library entry's does not. srf-011 lowered twice:
/// as it is (`jsonb_array_elements`, a library entry) and with its function made one no entry serves (9999, a scalar jsonb
/// set by the pg_proc seam: an internal function, so `FmgrSet`). E2e m5_pset_private runs the sessions. D787 (V4R-295):
/// srf-009's `three()` (16404, a VOLATILE SQL-language set in the target list) lowers through the same fmgr set, private,
/// where it refused by name as the srf.pl target-list form (e2e m5_pset_pl runs it).
#[test]
fn a_function_manager_set_is_marked_private_and_a_library_entry_is_not() {
    use gravity_kernels::ops::srf::entries::SETFN_PRIVATE;
    syscache_seams::pg_proc_proname::set(|_| Ok(None));
    syscache_seams::lookup_pg_proc_shape::set(|f| {
        // 9999: an internal immutable jsonb set; 16404: srf-009's three(), LANGUAGE sql VOLATILE, int4
        let (prorettype, prolang, pronargs, provolatile) = match f {
            9999 => (3802, 12, 1, b'i'),
            16404 => (23, 14, 0, b'v'),
            _ => return Ok(None),
        };
        Ok(Some(syscache_seams::PgProcShape {
            pronamespace: 11, prorettype, provariadic: 0, prosupport: 0, prolang, pronargs, prokind: b'f' as i8,
            provolatile: provolatile as i8, proparallel: b's' as i8, proretset: true, proisstrict: true, proleakproof: false,
            prosecdef: false, proconfig_isnull: true,
        }))
    });
    // FmgrSet::check resolves the result row as ExecInitFunctionScan does: jsonb's and int4's pg_type rows (their shapes from the
    // rig's install_seams, pg_type.dat, D1191)
    syscache_seams::pg_type_typtype::set(|t| Ok(matches!(t, 3802 | 23).then_some(b'b' as i8)));
    let lib = text("srf-011.plan");
    for (t, private) in [(lib.replace(":funcid 3219 ", ":funcid 9999 "), true), (lib, false)] {
        let l = common::lower_fixture(&t).expect("decomposes").unwrap_or_else(|r| panic!("private {private}: {r}"));
        let arg = l.descs.v4.protos.iter().find(|p| p.kind == gravity_program::protos::ProtoKind::SetFn).expect("a SetFn protocol").arg;
        assert_eq!(arg & SETFN_PRIVATE != 0, private, "srf-011, fmgr {private}: SETFN_PRIVATE");
    }
    let l = common::lower_fixture(&text("srf-009.plan")).expect("decomposes").unwrap_or_else(|r| panic!("srf-009: {r}"));
    let arg = l.descs.v4.protos.iter().find(|p| p.kind == gravity_program::protos::ProtoKind::SetFn).expect("srf-009: a SetFn protocol").arg;
    assert!(arg & SETFN_PRIVATE != 0, "srf-009: a SQL-language set is SETFN_PRIVATE");
}

//! The lowering's program inputs read from the session (the inputs home, rule C8; construct bundle core-face-09): the GUC
//! reads the lowering makes, each named for its option.

/// `gravity_road_off`'s raw value (road_off.rs `read`, at the top of every lowering): None with no option table (the seam
/// uninstalled: unit tests, tools); a failed read is the statement's error, raised as PostgreSQL's.
pub(crate) fn road_off() -> Option<Option<String>> {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return None;
    }
    Some(guc_seams::get_config_option_missing_ok::call(crate::road_off::ROAD_OFF_GUC).unwrap_or_else(|e| std::panic::panic_any(e)))
}

/// `pgrust.gravity_front_check` (frame/program.rs `verify`): on, a lowering verifies its IR again even when the verified-IR
/// slot holds its key. Off when the GUC seam is not installed (a unit test); a value the seam cannot read counts as on.
/// Not a program input: it decides whether a check around the program runs again, never the program.
/// `pgrust.gravity_effects_agree` (effects/end_check.rs): `report` prints each disagreement of the three expression readers
/// after a lowering, `assert` also fails a debug build at an unbooked one; off when unset, unreadable or the GUC seam is
/// not installed (a unit test). Not a program input: it decides whether a check runs after the lowering, never the program.
pub(crate) fn effects_agree() -> crate::effects::end_check::Mode {
    use crate::effects::end_check::Mode;
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return Mode::Off;
    }
    match guc_seams::get_config_option_missing_ok::call("pgrust.gravity_effects_agree") {
        Ok(Some(v)) => {
            let t = v.trim();
            if t.eq_ignore_ascii_case("report") {
                Mode::Report
            } else if t.eq_ignore_ascii_case("assert") {
                Mode::Assert
            } else {
                Mode::Off
            }
        }
        _ => Mode::Off,
    }
}

pub(crate) fn verify_always() -> bool {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return false;
    }
    match guc_seams::get_config_option_missing_ok::call("pgrust.gravity_front_check") {
        Ok(v) => v.is_some_and(|v| {
            let t = v.trim();
            ["on", "true", "1", "yes"].iter().any(|&k| t.eq_ignore_ascii_case(k))
        }),
        Err(_) => true,
    }
}

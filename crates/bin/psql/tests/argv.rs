//! D4955: the psql binary reads its command line as C psql's getopt_long does (bundled short
//! options, an argument attached or in the next word, `--`). No server needed: `-V` exits before
//! connecting, and a socket directory that does not exist fails at connect time (exit 2), after
//! the command line was accepted; a refused command line exits 1.

use std::process::Command;

fn psql(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_psql"))
        .args(args)
        .env_remove("PGPASSWORD")
        .env("PSQL_INTERACTIVE", "0")
        .output()
        .expect("run psql");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const NOSOCK: &str = "/nonexistent-d4955-socket-dir";

#[test]
fn bundled_flags_reach_the_option_they_name() {
    let (rc, out, err) = psql(&["-XV"]);
    assert_eq!(rc, 0, "stderr: {err}");
    assert!(out.starts_with("psql (PostgreSQL) "), "stdout: {out}");
}

#[test]
fn a_bundle_ending_in_c_takes_the_next_word_as_the_command() {
    let (rc, _, err) = psql(&["-Atc", "select 1", "-h", NOSOCK, "-p", "1"]);
    assert!(!err.contains("option"), "command line refused: {err}");
    assert_eq!(rc, 2, "expected a connect failure, stderr: {err}");
}

#[test]
fn a_bundle_ending_in_c_takes_the_rest_of_the_word_as_the_command() {
    let (rc, _, err) = psql(&["-Atcselect 1", &format!("-h{NOSOCK}"), "-p1", "--", "postgres"]);
    assert!(!err.contains("option"), "command line refused: {err}");
    assert_eq!(rc, 2, "expected a connect failure, stderr: {err}");
}

#[test]
fn an_unknown_short_option_in_a_bundle_is_refused_as_c_does() {
    let (rc, _, err) = psql(&["-AZ", "-c", "select 1"]);
    assert_eq!(rc, 1);
    assert!(err.starts_with("psql: invalid option -- 'Z'\n"), "stderr: {err}");
}

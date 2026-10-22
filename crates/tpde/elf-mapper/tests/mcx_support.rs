// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const EXTERNAL_CASES: [&str; 8] = [
    "external_reservation_tracks_move_drop_and_reuse",
    "external_reservation_enforces_limits_without_failure_charge",
    "external_reservation_enforces_ancestor_limits",
    "external_reservation_rejects_local_overflow_and_saturates_subtree",
    "external_reservation_on_unlimited_path_does_not_touch_ancestors",
    "external_reservation_allocation_overflow_is_rejected",
    "external_reservation_preserves_backend_allocations",
    "external_reservation_unwinds_failed_external_operation",
];
const RETIRED_CASE: &str = "external_reservation_rejects_retired_context";

struct Harness {
    root: PathBuf,
    artifacts: PathBuf,
    target: PathBuf,
    cargo: PathBuf,
    rustc: PathBuf,
    rustdoc: PathBuf,
}

impl Harness {
    // The runner harness provides both directories; a plain box has neither, and this
    // test drives a nested cargo build, so it skips there (pgrust carry, 2026-09-16).
    fn available() -> bool {
        env::var_os("COMPONENT_ARTIFACT_DIR").is_some() && env::var_os("CARGO_TARGET_DIR").is_some()
    }

    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let artifacts = PathBuf::from(
            env::var_os("COMPONENT_ARTIFACT_DIR").expect("runner artifact directory required"),
        )
        .join("mcx-support");
        let target = PathBuf::from(
            env::var_os("CARGO_TARGET_DIR").expect("runner target directory required"),
        )
        .join("mcx-support");
        assert!(artifacts.is_absolute() && target.is_absolute());
        fs::create_dir_all(&artifacts).unwrap();
        fs::create_dir_all(&target).unwrap();
        let cargo = PathBuf::from(env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into()));
        assert!(
            cargo.is_absolute() && cargo.is_file(),
            "selected Cargo absent"
        );
        let bin = cargo.parent().unwrap();
        let rustc = bin.join("rustc");
        let rustdoc = bin.join("rustdoc");
        assert!(
            rustc.is_file() && rustdoc.is_file(),
            "selected tools absent"
        );
        Self {
            root,
            artifacts,
            target,
            cargo,
            rustc,
            rustdoc,
        }
    }

    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("CARGO_TARGET_DIR", &self.target)
            .env("RUSTC", &self.rustc)
            .env("RUSTDOC", &self.rustdoc)
            .env("CARGO_TERM_COLOR", "never")
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("MAKEFLAGS")
            .env_remove("MFLAGS");
        command
    }

    fn run(&self, name: &str, mut command: Command) -> String {
        fs::write(
            self.artifacts.join(format!("{name}.command.txt")),
            format!("cwd={:?}\ncommand={command:?}\n", self.root),
        )
        .unwrap();
        println!("MCX_SUPPORT_RUN {name}");
        let output = command.output().unwrap_or_else(|error| {
            fs::write(
                self.artifacts.join(format!("{name}.status.txt")),
                format!("spawn failed: {error}\n"),
            )
            .unwrap();
            panic!("{name}: {error}")
        });
        fs::write(
            self.artifacts.join(format!("{name}.stdout.log")),
            &output.stdout,
        )
        .unwrap();
        fs::write(
            self.artifacts.join(format!("{name}.stderr.log")),
            &output.stderr,
        )
        .unwrap();
        fs::write(
            self.artifacts.join(format!("{name}.status.txt")),
            format!("{}\n", output.status),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{name} failed: {}; inspect {}",
            output.status,
            self.artifacts.display()
        );
        String::from_utf8(output.stdout).expect("Cargo output must be UTF-8")
    }

    fn cargo(&self, verb: &str, profile: &str, target: &str) -> Command {
        let mut command = self.command(&self.cargo);
        command.args([
            verb,
            "--locked",
            "--offline",
            "--manifest-path",
            "crates/_support/mcx/Cargo.toml",
            "--package",
            "mcx",
            "--profile",
            profile,
            target,
        ]);
        command
    }

    fn source_identity(&self, name: &str) -> String {
        let mut paths = vec![
            PathBuf::from("Cargo.toml"),
            PathBuf::from("Cargo.lock"),
            PathBuf::from("rust-toolchain.toml"),
            PathBuf::from("crates/tpde/elf-mapper/tests/mcx_support.rs"),
        ];
        for directory in ["crates/_support/mcx", "crates/_support/types/types_error"] {
            source_files(&self.root, Path::new(directory), &mut paths);
        }
        if self.root.join(".cargo").is_dir() {
            source_files(&self.root, Path::new(".cargo"), &mut paths);
        }
        paths.sort();
        let mut command = self.command("sha256sum");
        command.arg("--").args(paths);
        self.run(name, command)
    }

    fn roster(&self, name: &str, profile: &str, target: &str, minimum: usize) -> BTreeSet<String> {
        let mut command = self.cargo("test", profile, target);
        command.args(["--", "--list", "--format", "terse"]);
        let output = self.run(&format!("{name}-list"), command);
        let mut roster = BTreeSet::new();
        for line in output.lines() {
            if let Some(test) = line.strip_suffix(": test") {
                assert!(
                    roster.insert(normalize_name(test).to_owned()),
                    "duplicate {test}"
                );
            }
        }
        assert!(
            roster.len() >= minimum,
            "{name}: only {} tests",
            roster.len()
        );
        fs::write(
            self.artifacts.join(format!("{name}.roster.txt")),
            roster.iter().cloned().collect::<Vec<_>>().join("\n") + "\n",
        )
        .unwrap();
        roster
    }

    fn execute(&self, name: &str, profile: &str, target: &str, roster: &BTreeSet<String>) {
        let mut command = self.cargo("test", profile, target);
        command.args(["--", "--format", "pretty", "--test-threads=1"]);
        let output = self.run(&format!("{name}-run"), command);
        let mut passed = BTreeSet::new();
        let mut summaries = 0;
        let mut total = 0;
        for line in output.lines() {
            if let Some(test) = line
                .strip_prefix("test ")
                .and_then(|s| s.strip_suffix(" ... ok"))
            {
                assert!(
                    passed.insert(normalize_name(test).to_owned()),
                    "duplicate {test}"
                );
            }
            if let Some(summary) = line.strip_prefix("test result: ok. ") {
                let (count, remainder) = summary.split_once(" passed; ").unwrap();
                assert!(
                    remainder.starts_with("0 failed; 0 ignored; 0 measured; 0 filtered out;"),
                    "{name}: skipped or failed tests: {summary}"
                );
                total += count.parse::<usize>().unwrap();
                summaries += 1;
            }
        }
        assert!(summaries > 0, "{name}: missing test summary");
        assert_eq!(
            total,
            roster.len(),
            "{name}: execution count differs from listing"
        );
        assert_eq!(
            &passed, roster,
            "{name}: executed roster differs from listing"
        );
        fs::write(
            self.artifacts.join(format!("{name}.result.txt")),
            format!(
                "PASS listed={} passed={total} ignored=0 filtered=0\n",
                roster.len()
            ),
        )
        .unwrap();
    }
}

fn source_files(root: &Path, relative: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root.join(relative)).unwrap() {
        let entry = entry.unwrap();
        let path = relative.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            source_files(root, &path, paths);
        } else {
            assert!(kind.is_file(), "non-file source: {}", path.display());
            paths.push(path);
        }
    }
}

fn normalize_name(name: &str) -> &str {
    name.strip_suffix(" - should panic")
        .or_else(|| name.strip_suffix(" - compile fail"))
        .unwrap_or(name)
}

fn external_cases(roster: &BTreeSet<String>, debug: bool) {
    let mut expected: BTreeSet<_> = EXTERNAL_CASES
        .iter()
        .map(|name| format!("tests::{name}"))
        .collect();
    if debug {
        expected.insert(format!("tests::{RETIRED_CASE}"));
    }
    let actual: BTreeSet<_> = roster
        .iter()
        .filter(|name| name.starts_with("tests::external_reservation_"))
        .cloned()
        .collect();
    assert_eq!(actual, expected, "external reservation roster changed");
}

#[test]
fn complete_mcx_unit_and_ownership_suites() {
    if !Harness::available() {
        eprintln!("skipped: COMPONENT_ARTIFACT_DIR / CARGO_TARGET_DIR not set (runner harness only)");
        return;
    }
    let harness = Harness::new();
    let mut revision = harness.command("git");
    revision.args(["rev-parse", "HEAD"]);
    let revision = harness.run("revision", revision);
    assert_eq!(revision.trim().len(), 40, "full source revision required");
    let source_before = harness.source_identity("source-before");
    for (name, tool) in [
        ("cargo-version", &harness.cargo),
        ("rustc-version", &harness.rustc),
        ("rustdoc-version", &harness.rustdoc),
    ] {
        let mut command = harness.command(tool);
        command.args(["--version", "--verbose"]);
        harness.run(name, command);
    }
    let recorded = [
        "RUSTUP_TOOLCHAIN",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_TARGET",
    ]
    .map(|name| format!("{name}={:?}\n", env::var_os(name)))
    .concat();
    fs::write(
        harness.artifacts.join("inherited-build-environment.txt"),
        recorded,
    )
    .unwrap();
    for name in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_TARGET",
    ] {
        assert!(
            env::var_os(name).is_none_or(|value| value.is_empty()),
            "ordinary mcx suites require no inherited {name} override"
        );
    }
    // The offline builds below need every locked dev-dependency in the local
    // registry (mcx's mimalloc dev-dependency builds through `cc`, which the
    // runner image does not pre-populate); fetch the locked graph once first.
    let mut fetch = harness.command(&harness.cargo);
    fetch.args([
        "fetch",
        "--locked",
        "--manifest-path",
        "crates/_support/mcx/Cargo.toml",
    ]);
    harness.run("fetch", fetch);
    for profile in ["dev", "dist"] {
        let mut command = harness.cargo("test", profile, "--lib");
        command.arg("--no-run");
        harness.run(&format!("{profile}-precompile"), command);
    }
    harness.run(
        "docs-library-precompile",
        harness.cargo("build", "dist", "--lib"),
    );
    let debug = harness.roster("debug", "dev", "--lib", 115);
    let dist = harness.roster("dist", "dist", "--lib", 100);
    let docs = harness.roster("docs", "dist", "--doc", 7);
    external_cases(&debug, true);
    external_cases(&dist, false);
    let guard_docs = docs
        .iter()
        .filter(|name| name.contains("external_allocation_guard::ExternalAllocationGuard (line "))
        .count();
    assert_eq!(
        guard_docs, 4,
        "all ExternalAllocationGuard doctests required"
    );
    harness.execute("debug", "dev", "--lib", &debug);
    harness.execute("dist", "dist", "--lib", &dist);
    harness.execute("docs", "dist", "--doc", &docs);
    assert_eq!(
        source_before,
        harness.source_identity("source-after"),
        "sources changed during tests"
    );
    let verdict = format!(
        "TPDE_MCX_SUPPORT PASS debug={} dist={} docs={} external_debug=9 external_dist=8 guard_docs=4 revision={}\n",
        debug.len(), dist.len(), docs.len(), revision.trim()
    );
    fs::write(harness.artifacts.join("verdict.txt"), &verdict).unwrap();
    print!("{verdict}");
}

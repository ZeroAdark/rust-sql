// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tpde_disarm_gen::{generate, Generated, Options};

const DESC: &str = include_str!("../reference/desc.txt");
const FEATURES: &str = include_str!("../reference/feat.txt");

fn python() -> Command {
    let mut cmd = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
    cmd.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/oracle.py"));
    cmd
}

fn work(name: &str) -> PathBuf {
    let root = std::env::var_os("TPDE_DISARM_GEN_ARTIFACT_DIR")
        .or_else(|| std::env::var_os("COMPONENT_ARTIFACT_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!("disarm-gen-{name}-{}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn same(actual: &[u8], expected: &[u8], case: &str) {
    let mismatch = actual.iter().zip(expected).position(|(a, b)| a != b);
    assert!(
        mismatch.is_none() && actual.len() == expected.len(),
        "{case}: first mismatch {:?}, Rust length {}, reference length {}",
        mismatch.or_else(
            || (actual.len() != expected.len()).then_some(actual.len().min(expected.len()))
        ),
        actual.len(),
        expected.len()
    );
}

fn options(profile: &str, coverage: bool, encode_in_header: bool) -> Options {
    Options {
        features: (profile != "default").then(|| profile.to_owned()),
        coverage,
        encode_in_header,
    }
}

fn reference(dir: &Path, profile: &str, opts: &Options, model: bool) {
    let mut command = python();
    command
        .arg("generate")
        .arg(dir)
        .arg("--features")
        .arg(profile);
    if opts.coverage {
        command.arg("--coverage");
    }
    if opts.encode_in_header {
        command.arg("--encode-in-header");
    }
    if model {
        command.args(["--model", "--classifications"]);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{profile}: Python oracle failed: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
}

fn compare(generated: &Generated, dir: &Path, case: &str) {
    same(
        generated.reference_public.as_bytes(),
        &fs::read(dir.join("public.inc")).unwrap(),
        &format!("{case} public"),
    );
    same(
        generated.reference_private.as_bytes(),
        &fs::read(dir.join("private.inc")).unwrap(),
        &format!("{case} private"),
    );
    let manifest: String = generated
        .manifest
        .lines()
        .filter(|line| {
            [
                "feature\t",
                "group\t",
                "mnemonic\t",
                "trie_desc\t",
                "decstr\t",
                "form\t",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    same(
        manifest.as_bytes(),
        &fs::read(dir.join("manifest.tsv")).unwrap(),
        &format!("{case} model"),
    );
}

#[test]
fn all_profiles_and_output_modes_match_pinned_python() {
    let output = python().arg("profiles").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profiles = String::from_utf8(output.stdout).unwrap();
    let root = work("profiles");
    let mut count = 0;
    for (index, profile) in profiles.lines().enumerate() {
        let opts = options(profile, false, false);
        let dir = root.join(index.to_string());
        reference(&dir, profile, &opts, false);
        let generated = generate(&[DESC], Some(FEATURES), &opts)
            .unwrap_or_else(|error| panic!("{profile}: {error}"));
        compare(&generated, &dir, profile);
        count += 1;
    }
    for profile in ["all", "really-all"] {
        for (coverage, header) in [(false, true), (true, false), (true, true)] {
            let name = format!("{profile}-{coverage}-{header}");
            let dir = root.join(&name);
            let opts = options(profile, coverage, header);
            reference(&dir, profile, &opts, false);
            compare(
                &generate(&[DESC], Some(FEATURES), &opts).unwrap(),
                &dir,
                &name,
            );
            count += 1;
        }
    }
    println!("TPDE_DISARM_GEN_ORACLE_OK cases={count}");
}

const TINY: &str = "[BASE]\ndesc: 00000000000000000000000000000000\nmnem: BASE\ndecode: base\nencmnem: BASE\nencops:\n";
const FEATURE_CASE: &str = "[A]\navail: armv8.1a\n[B]\navail: armv8.6a\nincomplete: yes\n[C]\navailopt: armv8.0a\nrequires: A\n[D]\navail: armv9.1a\n";

#[test]
fn cli_modes_and_failed_input_preserve_output_contract() {
    let root = work("cli");
    let source = root.join("input.ini");
    let public = root.join("public");
    let private = root.join("private");
    let manifest = root.join("manifest.tsv");
    fs::write(&source, TINY).unwrap();
    let mut cases = 0;
    for coverage in [false, true] {
        for encode_in_header in [false, true] {
            for c_output in [false, true] {
                let opts = Options {
                    coverage,
                    encode_in_header,
                    features: Some("ignored".into()),
                };
                let expected = generate(&[TINY], None, &opts).unwrap();
                let mut command = Command::new(env!("CARGO_BIN_EXE_disarm-gen"));
                command
                    .arg("--features=ignored")
                    .arg("--manifest")
                    .arg(&manifest);
                if coverage {
                    command.arg("--coverage");
                }
                if encode_in_header {
                    command.arg("--encode-in-header");
                }
                if c_output {
                    command.arg("--reference-c");
                }
                let result = command
                    .arg("--")
                    .arg(&public)
                    .arg(&private)
                    .arg(&source)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let (want_public, want_private) = if c_output {
                    (&expected.reference_public, &expected.reference_private)
                } else {
                    (&expected.public_rust, &expected.private_rust)
                };
                same(
                    &fs::read(&public).unwrap(),
                    want_public.as_bytes(),
                    "CLI public",
                );
                same(
                    &fs::read(&private).unwrap(),
                    want_private.as_bytes(),
                    "CLI private",
                );
                same(
                    &fs::read(&manifest).unwrap(),
                    expected.manifest.as_bytes(),
                    "CLI manifest",
                );
                cases += 1;
            }
        }
    }
    fs::write(&source, "[BAD]\ndesc: 0\n").unwrap();
    fs::write(&public, b"unchanged-public").unwrap();
    fs::write(&private, b"unchanged-private").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_disarm-gen"))
        .arg(&public)
        .arg(&private)
        .arg(&source)
        .output()
        .unwrap();
    assert!(!result.status.success());
    same(
        &fs::read(public).unwrap(),
        b"unchanged-public",
        "failed CLI public",
    );
    same(
        &fs::read(private).unwrap(),
        b"unchanged-private",
        "failed CLI private",
    );
    println!("TPDE_DISARM_GEN_CLI_OK cases={}", cases + 1);
}

fn cli_value_files(dir: &Path, feature_file: Option<&str>) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("input.ini"), TINY).unwrap();
    fs::write(dir.join("public"), b"unchanged-public").unwrap();
    fs::write(dir.join("private"), b"unchanged-private").unwrap();
    fs::write(dir.join("manifest.tsv"), b"unchanged-manifest").unwrap();
    if let Some(path) = feature_file {
        fs::write(dir.join(path), FEATURE_CASE).unwrap();
    }
}

fn cli_value_run(
    dir: &Path,
    args: &[String],
    trailing: bool,
    rust: bool,
    default_manifest: bool,
) -> std::process::Output {
    let mut command = if rust {
        let mut command = Command::new(env!("CARGO_BIN_EXE_disarm-gen"));
        command.arg("--reference-c");
        if default_manifest {
            command.args(["--manifest", "manifest.tsv"]);
        }
        command
    } else {
        let mut command =
            Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
        command.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("reference/parse.py"));
        command
    };
    command.current_dir(dir);
    if trailing {
        command.args(["public", "private", "input.ini"]).args(args);
    } else {
        command.args(args).args(["public", "private", "input.ini"]);
    }
    fs::write(
        dir.join("argv.txt"),
        format!("{:?}\n", command.get_args().collect::<Vec<_>>()),
    )
    .unwrap();
    let result = command.output().unwrap();
    fs::write(dir.join("stdout.txt"), &result.stdout).unwrap();
    fs::write(dir.join("stderr.txt"), &result.stderr).unwrap();
    result
}

fn cli_value_unchanged(dir: &Path) {
    for (file, contents) in [
        ("public", b"unchanged-public".as_slice()),
        ("private", b"unchanged-private".as_slice()),
        ("manifest.tsv", b"unchanged-manifest".as_slice()),
    ] {
        same(
            &fs::read(dir.join(file)).unwrap(),
            contents,
            &dir.display().to_string(),
        );
    }
}

fn cli_shared_value_case(
    root: &Path,
    case: usize,
    args: &[String],
    feature_file: Option<&str>,
    valid: bool,
    trailing: bool,
) {
    let rust = root.join(case.to_string()).join("rust");
    let reference = root.join(case.to_string()).join("reference");
    cli_value_files(&rust, feature_file);
    cli_value_files(&reference, feature_file);
    let actual = cli_value_run(&rust, args, trailing, true, true);
    let expected = cli_value_run(&reference, args, trailing, false, false);
    assert_eq!(
        expected.status.success(),
        valid,
        "CLI reference case {case} {args:?}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert_eq!(
        actual.status.success(),
        valid,
        "CLI Rust case {case} {args:?}: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    if valid {
        for file in ["public", "private"] {
            same(
                &fs::read(rust.join(file)).unwrap(),
                &fs::read(reference.join(file)).unwrap(),
                &format!("CLI shared case {case} {file}"),
            );
        }
        assert_ne!(
            fs::read(rust.join("manifest.tsv")).unwrap(),
            b"unchanged-manifest"
        );
    } else {
        cli_value_unchanged(&rust);
    }
}

fn cli_manifest_value_case(
    root: &Path,
    case: usize,
    args: &[String],
    manifest_path: Option<&str>,
    trailing: bool,
) {
    let dir = root.join(case.to_string());
    cli_value_files(&dir, None);
    if let Some(path) = manifest_path {
        fs::write(dir.join(path), b"unchanged-selected-manifest").unwrap();
    }
    let result = cli_value_run(&dir, args, trailing, true, false);
    assert_eq!(
        result.status.success(),
        manifest_path.is_some(),
        "CLI manifest case {case} {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    if let Some(path) = manifest_path {
        let manifest = fs::read_to_string(dir.join(path)).unwrap();
        assert!(
            manifest
                .lines()
                .any(|line| line.starts_with("emitted\tBASE\tBASE\t")),
            "CLI manifest case {case}: selected destination lacks generated manifest"
        );
        for file in ["public", "private"] {
            assert_ne!(
                fs::read(dir.join(file)).unwrap(),
                format!("unchanged-{file}").as_bytes()
            );
        }
    } else {
        cli_value_unchanged(&dir);
    }
}

fn cli_collect_case(
    failures: &mut Vec<String>,
    label: String,
    run: impl FnOnce() + std::panic::UnwindSafe,
) {
    if std::panic::catch_unwind(run).is_err() {
        failures.push(label);
    }
}

#[test]
fn cli_option_values_match_pinned_argparse() {
    let preflight = python().arg("preflight").output().unwrap();
    assert!(
        preflight.status.success(),
        "{}",
        String::from_utf8_lossy(&preflight.stderr)
    );
    let root = fs::canonicalize(work("cli-values")).unwrap();
    let mut failures = Vec::new();
    let rejected = [
        "--coverage",
        "--encode-in-header",
        "--features",
        "--feature-desc",
        "--manifest",
        "--reference-c",
        "--help",
        "-h",
        "-h text",
        "--unknown",
        "-unknown",
        "--",
        "--coverage=on",
        "--coverage=x y",
        "-unknown\ttext",
        "-1e3",
        "-1.",
        "-1tail",
    ];
    let accepted = [
        "plain",
        "-",
        "-1",
        "-1.5",
        "-.5",
        "-0.0",
        "-1\n",
        "--unknown text",
        "path=value",
    ];
    let explicit = [
        "plain",
        "--coverage",
        "-unknown",
        "--",
        "-",
        "-1",
        "-.5",
        "path=value",
    ];
    let mut shared = 0;
    for option in ["--features", "--feature-desc"] {
        for value in rejected {
            cli_collect_case(&mut failures, format!("shared-{shared}"), || {
                cli_shared_value_case(
                    &root,
                    shared,
                    &[option.into(), value.into()],
                    None,
                    false,
                    false,
                );
            });
            shared += 1;
        }
        for value in accepted {
            cli_collect_case(&mut failures, format!("shared-{shared}"), || {
                cli_shared_value_case(
                    &root,
                    shared,
                    &[option.into(), value.into()],
                    (option == "--feature-desc").then_some(value),
                    true,
                    false,
                );
            });
            shared += 1;
        }
        for value in explicit {
            cli_collect_case(&mut failures, format!("shared-{shared}"), || {
                cli_shared_value_case(
                    &root,
                    shared,
                    &[format!("{option}={value}")],
                    (option == "--feature-desc").then_some(value),
                    true,
                    false,
                );
            });
            shared += 1;
        }
        cli_collect_case(&mut failures, format!("shared-{shared}"), || {
            cli_shared_value_case(&root, shared, &[option.into()], None, false, true);
        });
        shared += 1;
    }
    for args in [
        vec!["--features".into(), "".into()],
        vec!["--features=".into()],
    ] {
        cli_collect_case(&mut failures, format!("shared-{shared}"), || {
            cli_shared_value_case(&root, shared, &args, None, true, false);
        });
        shared += 1;
    }
    for (args, feature_file) in [
        (vec!["--feature-desc", ""], None),
        (vec!["--feature-desc="], None),
        (
            vec!["--feature-desc", "features.ini", "--features=--"],
            Some("features.ini"),
        ),
        (
            vec![
                "--feature-desc",
                "features.ini",
                "--features",
                "a",
                "--features=--",
            ],
            Some("features.ini"),
        ),
        (
            vec!["--feature-desc", "features.ini", "--feature-desc=--"],
            Some("features.ini"),
        ),
        (
            vec!["--feature-desc=--", "--features", "unknown"],
            Some("--"),
        ),
        (vec!["--feature-desc=./--"], Some("./--")),
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        cli_collect_case(&mut failures, format!("shared-{shared}"), || {
            cli_shared_value_case(&root, shared, &args, feature_file, true, false);
        });
        shared += 1;
    }
    let manifest_root = root.join("manifest");
    let mut manifest = 0;
    for value in rejected {
        cli_collect_case(&mut failures, format!("manifest-{manifest}"), || {
            cli_manifest_value_case(
                &manifest_root,
                manifest,
                &["--manifest".into(), value.into()],
                None,
                false,
            );
        });
        manifest += 1;
    }
    for value in [
        "manifest.out",
        "-",
        "-1",
        "-1.5",
        "-.5",
        "-1\n",
        "--unknown text",
        "manifest=out",
    ] {
        cli_collect_case(&mut failures, format!("manifest-{manifest}"), || {
            cli_manifest_value_case(
                &manifest_root,
                manifest,
                &["--manifest".into(), value.into()],
                Some(value),
                false,
            );
        });
        manifest += 1;
    }
    for value in [
        "manifest.out",
        "--coverage",
        "-unknown",
        "--",
        "-",
        "-1",
        "-.5",
        "manifest=out",
    ] {
        cli_collect_case(&mut failures, format!("manifest-{manifest}"), || {
            cli_manifest_value_case(
                &manifest_root,
                manifest,
                &[format!("--manifest={value}")],
                Some(value),
                false,
            );
        });
        manifest += 1;
    }
    cli_collect_case(&mut failures, format!("manifest-{manifest}"), || {
        cli_manifest_value_case(&manifest_root, manifest, &["--manifest".into()], None, true);
    });
    manifest += 1;
    let exact = [
        "--feat",
        "--feature",
        "--man",
        "--cover",
        "--encode-in",
        "--ref",
        "--features-extra",
    ];
    for (case, option) in exact.iter().enumerate() {
        cli_collect_case(&mut failures, format!("exact-{case}"), || {
            cli_manifest_value_case(
                &root.join("exact-options"),
                case,
                &[format!("{option}=ignored")],
                None,
                false,
            );
        });
    }
    assert!(
        failures.is_empty(),
        "{} CLI value cases failed: {}",
        failures.len(),
        failures.join(", ")
    );
    println!(
        "TPDE_DISARM_GEN_CLI_VALUES_OK shared={shared} manifest={manifest} exact={} cases={}",
        exact.len(),
        shared + manifest + exact.len()
    );
}

fn custom_case(
    root: &Path,
    name: &str,
    desc: &[&str],
    features: Option<&str>,
    selected: &str,
    valid: bool,
) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let mut command = python();
    command
        .arg("generate")
        .arg(&dir)
        .arg("--features")
        .arg(selected);
    if let Some(features) = features {
        let path = dir.join("features.ini");
        fs::write(&path, features).unwrap();
        command.arg("--feature-desc").arg(path);
    } else {
        command.arg("--no-feature-desc");
    }
    for (index, source) in desc.iter().enumerate() {
        let path = dir.join(format!("desc-{index}.ini"));
        fs::write(&path, source).unwrap();
        command.arg("--desc").arg(path);
    }
    let reference = command.output().unwrap();
    assert_eq!(
        reference.status.success(),
        valid,
        "{name}: unexpected reference result: {}\n{}",
        reference.status,
        String::from_utf8_lossy(&reference.stderr)
    );
    let actual = generate(desc, features, &options(selected, false, false));
    if valid {
        compare(
            &actual.unwrap_or_else(|error| panic!("{name}: {error}")),
            &dir,
            name,
        );
    } else {
        assert!(
            actual.is_err(),
            "{name}: Rust unexpectedly accepted malformed input"
        );
    }
}

fn custom_ini_case(root: &Path, name: &str, source: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("input.ini");
    fs::write(&path, source).unwrap();
    let reference = python().arg("ini").arg(path).output().unwrap();
    assert!(
        reference.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let mut actual = Vec::new();
    for section in tpde_disarm_gen::parse_ini(&[source]).unwrap() {
        let mut fields = vec![section.name, section.props.len().to_string()];
        fields.extend(
            section
                .props
                .into_iter()
                .flat_map(|(key, value)| [key, value]),
        );
        for field in fields {
            actual.extend_from_slice(&(field.len() as u32).to_le_bytes());
            actual.extend_from_slice(field.as_bytes());
        }
    }
    fs::write(dir.join("rust.bin"), &actual).unwrap();
    fs::write(dir.join("reference.bin"), &reference.stdout).unwrap();
    same(&actual, &reference.stdout, name);
}

#[test]
fn synthetic_branches_and_errors_match_pinned_python() {
    let root = work("synthetic");
    let one_bit_group = format!("[G]\ndesc: {}{{1:x}}\n", "0".repeat(31));
    let high_one_bit_group = format!("[G]\ndesc: {}{{1:x}}\n", "1".repeat(31));
    let mut cases = 0;
    let mut failures = Vec::new();
    let mut check =
        |name: &str, desc: &[&str], features: Option<&str>, selected: &str, valid: bool| {
            cases += 1;
            if std::panic::catch_unwind(|| {
                custom_case(&root, name, desc, features, selected, valid)
            })
            .is_err()
            {
                failures.push(name.to_owned());
            }
        };
    check(
        "feature-option-ignored-without-description",
        &[TINY],
        None,
        "nonsense",
        true,
    );
    check(
        "multifile-override",
        &[TINY, "[BASE]\ndecode: replacement\n"],
        None,
        "default",
        true,
    );
    check(
        "defaults",
        &[
            "[DEFAULT]\ndecode: base\nencops:\n[BASE]\ndesc: 00000000000000000000000000000000\nmnem: BASE\nencmnem: BASE\n",
        ],
        None,
        "default",
        true,
    );
    check(
        "repeated-default-headers",
        &[
            "[DEFAULT]\ndecode: base\n[DEFAULT]\nencops:\n[BASE]\ndesc: 00000000000000000000000000000000\nmnem: BASE\nencmnem: BASE\n",
        ],
        None,
        "default",
        true,
    );
    check(
        "empty-section",
        &[
            "[]\ndesc: 00000000000000000000000000000000\nmnem: BASE\ndecode: base\nencmnem: BASE\nencops:\n",
        ],
        None,
        "default",
        false,
    );
    check(
        "duplicate-operand-options",
        &[
            "[G]\ndesc: 0000000000000000000000000000{4:x}\nmnem: G\ndecode: g OPuimm(@x)\nencmnem: G\nencops: x=imm/max=8,max=16\n",
        ],
        None,
        "default",
        true,
    );
    check(
        "duplicate-alias-last-wins",
        &[
            "[G]\ndesc: 00000000000000000000{4:x}{4:y}{4:z}\nmnem: G\ndecode: g OPuimm(@x)\nencmnem: G\nencops: x=imm/max=16 y=const/val=3 z=@absent z=@y\n",
        ],
        None,
        "default",
        true,
    );
    check(
        "cross-field-slice",
        &[
            "[G]\ndesc: 0000000000000000000000000000{2:a}{2:b}\nmnem: G{b<3:2>:0=zero:1=one:2=two:3=three}\ndecode: g OPuimm(@a)\nencmnem: G{b<3:2>:0=zero:1=one:2=two:3=three}\nencops: b=imm/max=4\n",
        ],
        None,
        "default",
        true,
    );
    check(
        "disabled-form-missing-operands-condition",
        &[
            TINY,
            &format!(
                "{high_one_bit_group}encmnem: G{{x:0=zero:1=one}}\nencops: {{x:0=}}\nenccond: {{x:0=}}\nencfeat: {{x:0=:1=A}}\n"
            ),
        ],
        Some(FEATURE_CASE),
        "c",
        true,
    );
    check(
        "enabled-form-absent-condition-is-empty",
        &[
            TINY,
            &format!(
                "{high_one_bit_group}encmnem: G{{x:0=zero:1=one}}\nencops:\nenccond: {{x:0=}}\n"
            ),
        ],
        None,
        "default",
        true,
    );
    check(
        "invalid-feature-without-generated-forms",
        &[
            TINY,
            &format!("[G]\ndesc: {}\nfeat: {{missing:0=A}}\n", "1".repeat(32)),
        ],
        None,
        "default",
        false,
    );
    check(
        "format-overwrite-negate",
        &[
            "[G]\ndesc: 000000000000000000000000000000{2:x}\nmnem: P{x:*=same:^1=one:!2}\ndecode: p OPuimm(@x)\nencmnem: E{x:0=zero:1=one:3=three}\nencops:\n",
        ],
        None,
        "default",
        true,
    );
    check(
        "format-extra-value",
        &[&format!(
            "{one_bit_group}mnem: P{{x:0,1=zero:1=one}}\ndecode: p\nencmnem: E{{x:0=zero:1=one}}\nencops:\n"
        )],
        None,
        "default",
        true,
    );
    check(
        "missing-encoder-field",
        &[&format!(
            "{one_bit_group}mnem: G\ndecode: g OPuimm(@x)\nencmnem: G\nencops:\n"
        )],
        None,
        "default",
        true,
    );
    check(
        "signed-field-alias",
        &[
            "[G]\ndesc: 00000000000000000000{4:x|s}{4:y}{4:z}\nmnem: G\ndecode: g OPuimm((@x + @y))\nencmnem: G\nencops: x=imm/max=16 y,z=@x,x\n",
        ],
        None,
        "default",
        true,
    );
    for profile in [
        "default",
        "all",
        "really-all",
        "armv9.0a",
        "armv9.1a",
        "c",
        "a,c",
        "",
    ] {
        check(
            &format!("custom-features-{profile}"),
            &[TINY],
            Some(FEATURE_CASE),
            profile,
            true,
        );
    }
    for profile in ["A", "a, c", "unknown", " "] {
        check(
            &format!("bad-feature-{}", profile.replace([' ', ','], "_")),
            &[TINY],
            Some(FEATURE_CASE),
            profile,
            false,
        );
    }
    for whitespace in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
        let desc = format!(
            "{whitespace}{}{whitespace}",
            TINY.replace(
                "decode: base",
                &format!("decode:{whitespace}base {whitespace}OPuimm({whitespace}1{whitespace})")
            )
        );
        check(
            &format!("python-whitespace-generation-{:x}", whitespace as u32),
            &[&desc],
            Some(&format!("{whitespace}{FEATURE_CASE}{whitespace}")),
            &format!("{whitespace}all{whitespace}"),
            true,
        );
    }
    for (name, newline) in [("bare-cr", "\r"), ("crlf", "\r\n")] {
        check(
            &format!("{name}-generation"),
            &[&TINY.replace('\n', newline)],
            Some(&FEATURE_CASE.replace('\n', newline)),
            "all",
            true,
        );
    }
    for (name, malformed) in [
        ("invalid-token", "?".to_owned()),
        ("short-descriptor", "0".repeat(31)),
        ("long-descriptor", "0".repeat(33)),
        ("bad-fixed-width", format!("{}{{2:x=0}}", "0".repeat(30))),
        ("bad-field-name", "{32:_bad}".to_owned()),
        ("zero-width", "{0:x}".to_owned()),
    ] {
        let source = TINY.replace("00000000000000000000000000000000", &malformed);
        check(name, &[&source], None, "default", false);
    }
    for (name, extra) in [
        ("duplicate-section", TINY),
        ("duplicate-property", "mnem: OTHER\n"),
    ] {
        check(name, &[&format!("{TINY}{extra}")], None, "default", false);
    }
    check(
        "invalid-format-controls-baseline",
        &[&format!(
            "{one_bit_group}mnem: G\ndecode: g OPuimm(@x)\nencmnem: G\nencops: x=bool\n"
        )],
        None,
        "default",
        true,
    );
    for (name, property, value) in [
        ("invalid-format", "mnem", "{x}"),
        ("duplicate-format-key", "mnem", "{x:0=A:0=B}"),
        ("overwrite-absent", "mnem", "{x:^0=A}"),
        ("negate-absent", "mnem", "{x:!0}"),
        ("overlapping-format-fields", "mnem", "{x,x<0>:*,*=A}"),
        ("format-range", "mnem", "{x:2=A}"),
        ("duplicate-assignment", "encops", "x=bool x=bool"),
        ("alias-arity", "encops", "x,y=@z"),
        ("unknown-operand", "encops", "x=nope"),
        ("ambiguous-decode", "decode", "{x:0=a:1=b}"),
    ] {
        let mut source = one_bit_group.clone();
        for (key, fallback) in [
            ("mnem", "G"),
            ("decode", "g OPuimm(@x)"),
            ("encmnem", "G"),
            ("encops", "x=bool"),
        ] {
            source.push_str(&format!(
                "{key}: {}\n",
                if property == key { value } else { fallback }
            ));
        }
        check(name, &[&source], None, "default", false);
    }
    for (name, source) in [
        ("greedy-section-close", "[BASE]suffix]\nkey: value\n"),
        (
            "unclosed-section-is-property",
            "[BASE]\n[extra: value\n[]: another\n",
        ),
        (
            "unicode-character-indentation",
            "[BASE]\n\u{2003}key: first\n  next: second\nlast: separate\n",
        ),
    ] {
        cases += 1;
        if std::panic::catch_unwind(|| custom_ini_case(&root, name, source)).is_err() {
            failures.push(name.to_owned());
        }
    }
    for whitespace in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
        let name = format!("python-whitespace-ini-{:x}", whitespace as u32);
        let source = format!(
            "{whitespace}; comment\n{whitespace}[DEFAULT]{whitespace}\n{whitespace}inherited{whitespace}:{whitespace}default{whitespace}\n{whitespace}[BASE]{whitespace}\n{whitespace}key{whitespace}:{whitespace}first{whitespace}\n{whitespace}{whitespace}continued{whitespace}\n{whitespace}\n{whitespace}{whitespace}third{whitespace}\ntail{whitespace}:{whitespace}last{whitespace}\ninterior: left{whitespace}right\n"
        );
        cases += 1;
        if std::panic::catch_unwind(|| custom_ini_case(&root, &name, &source)).is_err() {
            failures.push(name);
        }
    }
    for (name, newline) in [("bare-cr", "\r"), ("crlf", "\r\n")] {
        let name = format!("{name}-ini");
        let source =
            "[DEFAULT]\nbase: inherited\n[BASE]\nkey: first\n  continued\n\n  third\nlast: value\n"
                .replace('\n', newline);
        cases += 1;
        if std::panic::catch_unwind(|| custom_ini_case(&root, &name, &source)).is_err() {
            failures.push(name);
        }
    }
    for expression in [
        "OPuimm(UNDECLARED)",
        "OPuimm(external(1))",
        "OPuimm((char)255 < 0)",
    ] {
        let input = TINY.replace("decode: base", &format!("decode: base {expression}"));
        assert!(
            generate(&[&input], None, &Options::default()).is_err(),
            "untyped expression admitted: {expression}"
        );
    }
    let good = generate(&[TINY], None, &Options::default()).unwrap();
    let repeated = generate(&[TINY], None, &Options::default()).unwrap();
    assert_eq!(good.reference_public, repeated.reference_public);
    assert!(
        failures.is_empty(),
        "{} of {cases} synthetic cases failed: {}",
        failures.len(),
        failures.join(", ")
    );
    println!("TPDE_DISARM_GEN_SYNTHETIC_OK cases={cases}");
}

const RUNTIME: &str = r#"
mod runtime {
    #[derive(Clone, Copy)] pub struct GReg(pub u8);
    #[derive(Clone, Copy)] pub struct GRegZr(pub u8);
    #[derive(Clone, Copy)] pub struct GRegSp(pub u8);
    #[derive(Clone, Copy)] pub struct VReg(pub u8);
    impl GReg { pub fn value(self)->u8 { self.0 } }
    impl GRegZr { pub fn value(self)->u8 { self.0 } }
    impl GRegSp { pub fn value(self)->u8 { self.0 } }
    impl VReg { pub fn value(self)->u8 { self.0 } }
    pub type Cond = u32;
    pub type PrfOp = u32;
    pub fn da_sext(_:i64,_:u32)->i64 { panic!("runtime helper contract") }
    pub fn da_immadd(_:i64)->u32 { panic!("runtime helper contract") }
    pub fn da_immlogical(_:u64,_:u32)->u32 { panic!("runtime helper contract") }
    pub fn da_immfmov32(_:f32)->u32 { panic!("runtime helper contract") }
    pub fn da_immfmov64(_:f64)->u32 { panic!("runtime helper contract") }
    pub fn da_immsimdmovi(_:u64)->u32 { panic!("runtime helper contract") }
}
"#;

const CLASSIFY_MAIN: &str = r#"
fn main() {
    use std::io::{Read, Write};
    assert_eq!(generated::de64_ADCw(runtime::GRegZr(1),runtime::GRegZr(2),runtime::GRegZr(3)),0x1a030041);
    assert_eq!(generated::de64_UDF(0x1234),0x1234);
    assert_eq!(generated::de64_UDF(0x10000),0);
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert_eq!(input.len()%4,0);
    let mut output = Vec::with_capacity(input.len());
    for word in input.chunks_exact(4) {
        let value = generated::da64_classify_impl(u32::from_le_bytes(word.try_into().unwrap()));
        output.extend_from_slice(&value.to_le_bytes());
    }
    std::io::stdout().write_all(&output).unwrap();
}
"#;

const CLASSIFY_CPP: &str = r#"
#include <cstdint>
#include <cstdio>
#define DA64_CLASSIFIER
#include "private.inc"
int main() {
    unsigned char bytes[4];
    while (std::fread(bytes,1,4,stdin)==4) {
        uint32_t word=uint32_t(bytes[0])|(uint32_t(bytes[1])<<8)|(uint32_t(bytes[2])<<16)|(uint32_t(bytes[3])<<24);
        uint32_t value=da64_classify_impl(word);
        for (unsigned i=0;i<4;i++) bytes[i]=value>>(8*i);
        if (std::fwrite(bytes,1,4,stdout)!=4) return 1;
    }
    return std::ferror(stdin)?2:0;
}
"#;

#[test]
fn generated_rust_compiles_and_classifier_matches_compiled_upstream() {
    let root = work("generated");
    let mut count = 0;
    let mut words = 0;
    for profile in ["all", "really-all"] {
        for coverage in [false, true] {
            for header in [false, true] {
                let name = format!("{profile}-{coverage}-{header}");
                let dir = root.join(&name);
                let opts = options(profile, coverage, header);
                reference(&dir, profile, &opts, true);
                let generated = generate(&[DESC], Some(FEATURES), &opts).unwrap();
                compare(&generated, &dir, &name);
                let repeat = generate(&[DESC], Some(FEATURES), &opts).unwrap();
                assert_eq!(
                    generated.public_rust, repeat.public_rust,
                    "{name} public determinism"
                );
                assert_eq!(
                    generated.private_rust, repeat.private_rust,
                    "{name} private determinism"
                );
                assert_eq!(
                    generated.manifest, repeat.manifest,
                    "{name} manifest determinism"
                );
                fs::write(dir.join("public.rs"), &generated.public_rust).unwrap();
                fs::write(dir.join("private.rs"), &generated.private_rust).unwrap();
                fs::write(dir.join("manifest-rust.tsv"), &generated.manifest).unwrap();
                let source = format!(
                    "{RUNTIME}\nmod generated {{ include!({:?}); include!({:?}); }}\n{CLASSIFY_MAIN}",
                    dir.join("public.rs"),
                    dir.join("private.rs")
                );
                fs::write(dir.join("consumer.rs"), source).unwrap();
                let executable = dir.join("consumer");
                let compiled =
                    Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                        .args(["--edition=2024", "-A", "warnings"])
                        .arg(dir.join("consumer.rs"))
                        .arg("-o")
                        .arg(&executable)
                        .output()
                        .unwrap();
                assert!(
                    compiled.status.success(),
                    "{name}: generated Rust compile failed: {}\n{}",
                    compiled.status,
                    String::from_utf8_lossy(&compiled.stderr)
                );
                let input_path = dir.join("classification.bin");
                let rust = Command::new(&executable)
                    .stdin(Stdio::from(fs::File::open(&input_path).unwrap()))
                    .output()
                    .unwrap();
                assert!(
                    rust.status.success(),
                    "{name}: generated consumer failed: {}\n{}",
                    rust.status,
                    String::from_utf8_lossy(&rust.stderr)
                );
                fs::write(dir.join("classifier.cpp"), CLASSIFY_CPP).unwrap();
                let cpp = dir.join("classifier");
                oracle::compile_cpp(&dir, &cpp, &[dir.join("classifier.cpp")], true);
                let input = fs::read(input_path).unwrap();
                oracle::compare_bytes(&cpp, &dir, &input, &rust.stdout, &name);
                words += input.len() / 4;
                count += 1;
            }
        }
    }
    println!("TPDE_DISARM_GEN_GENERATED_RUST_OK cases={count} classifier_words={words}");
}

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(executable: &Path, mode: &str) -> Output {
    Command::new(executable)
        .arg(mode)
        .output()
        .expect("run base witness")
}

fn rustc(
    crate_path: &Path,
    work: &Path,
    logging: bool,
    assertions: bool,
    abort: bool,
) -> std::path::PathBuf {
    let compiler = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let library = work.join("libtpde_base.rlib");
    let executable = work.join("base-rust");
    for is_library in [true, false] {
        let mut command = Command::new(&compiler);
        command.args([
            "--edition=2021",
            "-C",
            "opt-level=3",
            "-C",
            if assertions {
                "debug-assertions=yes"
            } else {
                "debug-assertions=no"
            },
            "-C",
            if abort { "panic=abort" } else { "panic=unwind" },
        ]);
        if logging {
            command.args(["--cfg", "feature=\"logging\""]);
        }
        if is_library {
            command
                .args(["--crate-name", "tpde_base", "--crate-type", "rlib"])
                .arg(crate_path.join("src/lib.rs"))
                .arg("-o")
                .arg(&library);
        } else {
            command
                .args(["--crate-name", "base_matrix", "--extern"])
                .arg(format!("tpde_base={}", library.display()))
                .arg(crate_path.join("tests/matrix.rs.inc"))
                .arg("-o")
                .arg(&executable);
        }
        let result = command.output().expect("compile Rust feature witness");
        assert!(
            result.status.success(),
            "Rust feature witness failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    executable
}

#[test]
fn pinned_base_feature_and_failure_matrix() {
    let crate_path = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = std::env::temp_dir().join(format!("tpde-base-matrix-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let include = crate_path.join("tests/upstream/tpde/include");
    let spdlog = crate_path.join("tests/upstream/deps/spdlog/include");
    let mut combinations = 0;
    let mut fatal_cases = 0;
    let mut unreachable_cases = 0;
    for logging in [false, true] {
        for assertions in [false, true] {
            for abort in [false, true] {
                let work = root.join(format!("{logging}-{assertions}-{abort}"));
                fs::create_dir_all(&work).unwrap();
                let wrapper = work.join("oracle.cpp");
                fs::write(
                    &wrapper,
                    format!(
                        "#define TPDE_BASE_LOGGING {}\n#include \"{}\"\n",
                        u8::from(logging),
                        crate_path.join("tests/oracle.cpp").display()
                    ),
                )
                .unwrap();
                let cpp = work.join("base-cpp");
                let mut flags = vec!["-stdlib=libc++", "-I", spdlog.to_str().unwrap()];
                if abort {
                    flags.push("-fno-exceptions");
                }
                oracle::compile_cpp_with_flags(&include, &cpp, &[wrapper], assertions, &flags);
                let rust = rustc(crate_path, &work, logging, assertions, abort);
                let normal = run(&rust, "normal");
                assert!(normal.status.success(), "Rust normal witness: {:?}", normal);
                oracle::compare_bytes(&cpp, &work, &[], &normal.stdout, "base normal matrix");
                combinations += 1;
                for mode in ["fatal", "fatal-off"] {
                    let expected = run(&cpp, mode);
                    let actual = run(&rust, mode);
                    assert_eq!(
                        actual.stdout, expected.stdout,
                        "fatal log/payload differs for {mode} in {work:?}"
                    );
                    if abort {
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            assert_eq!(expected.status.signal(), Some(6));
                            assert_eq!(actual.status.signal(), Some(6));
                        }
                        assert!(!expected.status.success() && !actual.status.success());
                    } else {
                        assert!(
                            expected.status.success() && actual.status.success(),
                            "fatal catch failed: C++={expected:?}, Rust={actual:?}"
                        );
                    }
                    fatal_cases += 1;
                }
                if assertions {
                    let expected = run(&cpp, "unreachable");
                    let actual = run(&rust, "unreachable");
                    assert!(!expected.status.success() && !actual.status.success());
                    assert!(
                        String::from_utf8_lossy(&expected.stderr).contains("unreachable detail")
                    );
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        assert_eq!(expected.status.signal(), Some(6));
                        assert_eq!(actual.status.signal(), Some(6));
                    }
                    unreachable_cases += 1;
                } else {
                    let actual = run(&rust, "unreachable");
                    assert!(!actual.status.success());
                    unreachable_cases += 1;
                }
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
    assert_eq!((combinations, fatal_cases, unreachable_cases), (8, 16, 8));
    println!(
        "TPDE_BASE_ORACLE PASS configurations=8 thresholds=7 fatal_cases=16 unreachable_cases=8"
    );
}

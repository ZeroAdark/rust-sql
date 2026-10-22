// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::Scenario;
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

core::arch::global_asm!(
    r#"
.text
.p2align 2
.global tpde_a64_platform_preservation_probe
.type tpde_a64_platform_preservation_probe, %function
tpde_a64_platform_preservation_probe:
    sub sp, sp, #192
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    stp d8, d9, [sp, #96]
    stp d10, d11, [sp, #112]
    stp d12, d13, [sp, #128]
    stp d14, d15, [sp, #144]
    stp x0, x1, [sp, #160]
    mov x9, sp
    str x9, [sp, #176]
    mov x19, #0x119
    mov x20, #0x120
    mov x21, #0x121
    mov x22, #0x122
    mov x23, #0x123
    mov x24, #0x124
    mov x25, #0x125
    mov x26, #0x126
    mov x27, #0x127
    mov x28, #0x128
    mov x29, #0x129
    mov x9, #0x208
    fmov d8, x9
    mov x9, #0x209
    fmov d9, x9
    mov x9, #0x210
    fmov d10, x9
    mov x9, #0x211
    fmov d11, x9
    mov x9, #0x212
    fmov d12, x9
    mov x9, #0x213
    fmov d13, x9
    mov x9, #0x214
    fmov d14, x9
    mov x9, #0x215
    fmov d15, x9
    ldr x16, [sp, #160]
    mov x0, #0
    mov x1, #0
    blr x16
    ldr x9, [sp, #168]
    str x0, [x9, #0]
    stp x19, x20, [x9, #8]
    stp x21, x22, [x9, #24]
    stp x23, x24, [x9, #40]
    stp x25, x26, [x9, #56]
    stp x27, x28, [x9, #72]
    str x29, [x9, #88]
    stp d8, d9, [x9, #96]
    stp d10, d11, [x9, #112]
    stp d12, d13, [x9, #128]
    stp d14, d15, [x9, #144]
    ldr x10, [sp, #176]
    str x10, [x9, #160]
    mov x10, sp
    str x10, [x9, #168]
    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    ldp d8, d9, [sp, #96]
    ldp d10, d11, [sp, #112]
    ldp d12, d13, [sp, #128]
    ldp d14, d15, [sp, #144]
    add sp, sp, #192
    ret
.size tpde_a64_platform_preservation_probe, .-tpde_a64_platform_preservation_probe
"#
);

unsafe extern "C" {
    fn tpde_a64_platform_preservation_probe(function: usize, output: *mut u64);
}

pub unsafe fn invoke_preserving_host(function: usize) -> u64 {
    let mut output = [0u64; 22];
    // SAFETY: the caller provides a live AAPCS function; the probe writes exactly 22 output words and restores host registers.
    unsafe { tpde_a64_platform_preservation_probe(function, output.as_mut_ptr()) };
    assert_eq!(
        &output[1..12],
        &[0x119, 0x120, 0x121, 0x122, 0x123, 0x124, 0x125, 0x126, 0x127, 0x128, 0x129],
        "callee-saved GP register corruption"
    );
    assert_eq!(
        &output[12..20],
        &[0x208, 0x209, 0x210, 0x211, 0x212, 0x213, 0x214, 0x215],
        "callee-saved FP register corruption"
    );
    assert_eq!(output[20], output[21], "callee did not restore SP");
    output[0]
}

fn roster_path() -> PathBuf {
    std::env::var_os("TPDE_A64_PLATFORM_ROSTER")
        .expect("native child must have an explicit roster path")
        .into()
}

pub fn record_pending(scenario: Scenario) {
    fs::write(
        roster_path().with_extension("pending.txt"),
        format!("{scenario:?}\n"),
    )
    .unwrap();
}

pub fn record_pass(scenario: Scenario, result: u64) {
    let mut file = OpenOptions::new().append(true).open(roster_path()).unwrap();
    writeln!(file, "{scenario:?}\t{result:016x}\tPASS").unwrap();
    file.flush().unwrap();
}

pub fn drive() {
    assert!(
        std::env::var_os("TPDE_A64_PLATFORM_CHILD").is_none(),
        "recursive native driver"
    );
    let root = std::env::var_os("COMPONENT_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = root.join(format!(
        "a64-platform-{}-{}",
        if cfg!(tpde_address_sanitizer) {
            "asan"
        } else {
            "native"
        },
        std::process::id()
    ));
    fs::create_dir(&directory).expect("fresh native platform artifact directory");
    let roster = directory.join("native-roster.tsv");
    fs::write(&roster, "scenario\tresult\tverdict\n").unwrap();
    let stdout = directory.join("child.stdout");
    let stderr = directory.join("child.stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--skip",
            "native_platform_scenarios_execute_in_bounded_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TPDE_A64_PLATFORM_CHILD", "1")
        .env("TPDE_A64_PLATFORM_ROSTER", &roster)
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
    fs::write(
        directory.join("child.command.txt"),
        format!("{command:?}\n"),
    )
    .unwrap();
    let mut child = command.spawn().expect("spawn native platform child");
    let deadline = Instant::now() + Duration::from_secs(180);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().expect("reap timed-out native child");
            fs::write(
                directory.join("child.status.txt"),
                format!("TIMEOUT {status}\n"),
            )
            .unwrap();
            panic!(
                "native platform child exceeded 180 seconds: {}",
                directory.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    fs::write(directory.join("child.status.txt"), format!("{status}\n")).unwrap();
    assert!(
        status.success(),
        "native platform child failed: {status}; artifacts {}",
        directory.display()
    );
    let stdout = fs::read_to_string(stdout).unwrap();
    let stderr = fs::read_to_string(stderr).unwrap();
    assert!(
        stdout.contains(
            "test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out"
        ),
        "incomplete native test roster: {stdout}"
    );
    for diagnostic in [
        "ERROR: AddressSanitizer",
        "runtime error:",
        "UndefinedBehaviorSanitizer",
    ] {
        assert!(
            !stderr.contains(diagnostic),
            "native child sanitizer failure: {stderr}"
        );
    }
    let roster = fs::read_to_string(roster).unwrap();
    let rows: Vec<_> = roster.lines().skip(1).collect();
    assert_eq!(rows.len(), 597, "incomplete native scenario roster");
    let mut names = BTreeSet::new();
    for row in rows {
        let columns: Vec<_> = row.split('\t').collect();
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[2], "PASS");
        assert!(names.insert(columns[0]), "duplicate native scenario");
    }
    let summary = "TPDE_COMPILER_A64_PLATFORM PASS tests=10 native_cases=597 preserved_gp=11 preserved_fp=8 sp=checked\n";
    fs::write(directory.join("verdict.txt"), summary).unwrap();
    print!("{summary}");
}

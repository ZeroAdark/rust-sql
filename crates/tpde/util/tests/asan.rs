// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{fs, path::Path, process::Command};
use tpde_util::asan::{poison_memory_region, unpoison_memory_region, ADDRESS_SANITIZER_ACTIVE};

#[cfg(tpde_address_sanitizer)]
extern "C" {
    fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
}

#[repr(align(64))]
struct Region([u8; 64]);

fn script() -> (Vec<u8>, Vec<u8>) {
    let commands: [[u8; 3]; 24] = [
        [0, 0, 0],
        [1, 64, 0],
        [0, 0, 64],
        [1, 16, 8],
        [0, 16, 8],
        [1, 0, 1],
        [1, 7, 1],
        [1, 63, 1],
        [1, 0, 64],
        [0, 1, 7],
        [1, 1, 2],
        [0, 5, 13],
        [1, 8, 16],
        [0, 9, 22],
        [1, 11, 3],
        [0, 0, 8],
        [1, 0, 64],
        [0, 56, 8],
        [1, 63, 1],
        [0, 32, 16],
        [1, 40, 8],
        [0, 40, 8],
        [1, 32, 16],
        [1, 0, 64],
    ];
    let mut region = Box::new(Region([0xa5; 64]));
    let pointer = region.0.as_mut_ptr();
    let mut input = Vec::new();
    let mut output = vec![u8::from(ADDRESS_SANITIZER_ACTIVE)];
    for command in commands {
        input.extend_from_slice(&command);
        // SAFETY: Each command stays inside this exclusively owned live allocation.
        unsafe {
            let start = pointer.add(command[1] as usize);
            if command[0] == 0 {
                poison_memory_region(start, command[2] as usize);
            } else {
                unpoison_memory_region(start, command[2] as usize);
            }
            for offset in 0..64 {
                #[cfg(tpde_address_sanitizer)]
                output.push(u8::from(
                    __asan_address_is_poisoned(pointer.add(offset)) != 0,
                ));
                #[cfg(not(tpde_address_sanitizer))]
                {
                    let _ = offset;
                    output.push(0);
                }
            }
        }
    }
    // SAFETY: Restore the complete live allocation before ordinary access or drop.
    unsafe {
        unpoison_memory_region(pointer, 64);
    }
    assert_eq!(region.0, [0xa5; 64]);
    if ADDRESS_SANITIZER_ACTIVE {
        assert!(output[1..].contains(&1));
        assert_eq!(&output[1 + 2 * 64..1 + 3 * 64], &[1; 64]);
        assert_eq!(&output[1 + 23 * 64..], &[0; 64]);
    } else {
        assert!(output.iter().all(|&byte| byte == 0));
    }
    (input, output)
}

#[test]
fn asan_child() {
    let Ok(mode) = std::env::var("TPDE_ASAN_CHILD") else {
        return;
    };
    assert!(mode == "poisoned" || mode == "recovered");
    let mut region = Box::new(Region([0xa5; 64]));
    let pointer = region.0.as_mut_ptr();
    // SAFETY: The allocation is live; the poisoned read intentionally tests ASAN in a subprocess.
    unsafe {
        poison_memory_region(pointer.add(16), 8);
        if mode == "recovered" {
            unpoison_memory_region(pointer.add(16), 8);
        }
        eprintln!("TPDE_ASAN_ACCESS armed=1");
        assert_eq!(pointer.add(16).read_volatile(), 0xa5);
        unpoison_memory_region(pointer, 64);
    }
    eprintln!("TPDE_ASAN_ACCESS survived=1");
}

fn access_witness(mut command: Command, poisoned: bool) {
    let result = command
        .env(
            "ASAN_OPTIONS",
            "detect_leaks=0:allow_user_poisoning=1:halt_on_error=1",
        )
        .output()
        .expect("run ASAN access witness");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("TPDE_ASAN_ACCESS armed=1"), "{stderr}");
    if ADDRESS_SANITIZER_ACTIVE && poisoned {
        assert!(
            !result.status.success(),
            "poisoned access unexpectedly survived"
        );
        assert!(
            stderr.contains("AddressSanitizer: use-after-poison"),
            "{stderr}"
        );
        assert!(!stderr.contains("TPDE_ASAN_ACCESS survived=1"), "{stderr}");
    } else {
        assert!(result.status.success(), "{stderr}");
        assert!(stderr.contains("TPDE_ASAN_ACCESS survived=1"), "{stderr}");
    }
}

#[test]
fn complete_address_sanitizer_matches_pinned_cpp() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-asan-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let (input, expected) = script();
    for assertions in [false, true] {
        let executable = work.join(if assertions {
            "oracle-debug"
        } else {
            "oracle-release"
        });
        let mut command = Command::new(std::env::var("CXX").unwrap_or_else(|_| "clang++".into()));
        command.args(["-std=c++20", "-O3"]);
        if !assertions {
            command.arg("-DNDEBUG");
        }
        if ADDRESS_SANITIZER_ACTIVE {
            command.arg("-fsanitize=address");
        }
        let result = command
            .arg("-I")
            .arg(root.join("tests/upstream-asan/tpde/include"))
            .arg(root.join("tests/asan_oracle.cpp"))
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("start C++ ASAN compiler");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        oracle::compare_bytes(&executable, &work, &input, &expected, "ASAN poison maps");
        if !assertions {
            for mode in ["poisoned", "recovered"] {
                let mut command = Command::new(&executable);
                command.arg(mode);
                access_witness(command, mode == "poisoned");
            }
        }
    }
    for mode in ["poisoned", "recovered"] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "asan_child", "--nocapture"])
            .env("TPDE_ASAN_CHILD", mode);
        access_witness(command, mode == "poisoned");
    }
    eprintln!(
        "TPDE_ASAN_ORACLE PASS active={} cpp_modes=2 operations=24 subprocess_cases=4",
        u8::from(ADDRESS_SANITIZER_ACTIVE)
    );
    fs::remove_dir_all(work).unwrap();
}

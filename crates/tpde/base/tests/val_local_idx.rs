// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;
use std::{fs, mem, path::Path};
use tpde_base::ValLocalIdx;

#[test]
fn pinned_index_layout_and_values() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-val-idx-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let mut expected = Vec::new();
    let mut out = |value: u64| expected.extend_from_slice(&value.to_le_bytes());
    out(mem::size_of::<ValLocalIdx>() as u64);
    out(mem::align_of::<ValLocalIdx>() as u64);
    for bit in 0..32 {
        for value in [1u32 << bit, !(1u32 << bit)] {
            let index = ValLocalIdx::new(value);
            out(u64::from(index.get()));
            out(u64::from(index == ValLocalIdx::new(value)));
            out(u64::from(index < ValLocalIdx::new(0x8000_0000)));
        }
    }
    out(u64::from(ValLocalIdx::new(0).get()));
    out(u64::from(ValLocalIdx::new(u32::MAX).get()));
    for assertions in [true, false] {
        let executable = work.join(format!("oracle-{assertions}"));
        oracle::compile_cpp(
            &base.join("tests/upstream/tpde/include"),
            &executable,
            &[base.join("tests/val_local_idx_oracle.cpp")],
            assertions,
        );
        oracle::compare_bytes(&executable, &work, &[], &expected, "ValLocalIdx");
    }
    fs::remove_dir_all(&work).unwrap();
    println!("TPDE_VAL_LOCAL_IDX_ORACLE PASS cases=66 cpp_modes=2 layout_metrics=2");
}

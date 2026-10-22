// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use mcx::MemoryContext;
use std::fs;
use std::path::Path;
#[path = "../../test-support/oracle.rs"]
mod oracle;
use tpde_string_table::StringTable;

fn record(output: &mut Vec<u8>, offset: usize, bytes: &[u8]) {
    output.extend_from_slice(&(offset as u64).to_le_bytes());
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(bytes);
}

#[test]
fn complete_operation_sequences_match_pinned_cpp() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work =
        std::env::temp_dir().join(format!("tpde-string-table-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let oracle = work.join("oracle");
    oracle::compile_cpp(
        &root.join("tests/upstream/tpde/include"),
        &oracle,
        &[
            root.join("tests/oracle.cpp"),
            root.join("tests/upstream/tpde/src/StringTable.cpp"),
            root.join("tests/upstream/tpde/src/util/SmallVector.cpp"),
        ],
        false,
    );
    let context = MemoryContext::new_bump("tpde-oracle");
    let mut table = StringTable::new(context.mcx());
    let mut input = Vec::new();
    let mut expected = Vec::new();
    let mut model = vec![0u8];
    record(&mut expected, 0, &model);
    let boundaries: [usize; 15] = [
        0, 1, 22, 23, 24, 25, 48, 49, 50, 98, 99, 100, 255, 256, 1024,
    ];
    for n in 0..512usize {
        let len = boundaries[n % boundaries.len()];
        let bytes: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(131) + n) as u8).collect();
        let prefix: Vec<u8> = (0..(n / 2) % 9).map(|i| (i * 73) as u8).collect();
        let (kind, prefix, bytes) = match n {
            0 => (0, vec![], vec![]),
            1 => (1, vec![], vec![]),
            2 | 3 => (0, vec![], vec![0, 255, 42]),
            4 => (1, vec![0, 254], vec![255, 0]),
            _ => ((n % 2) as u8, prefix, bytes),
        };
        input.push(kind);
        input.extend_from_slice(&(prefix.len() as u32).to_le_bytes());
        input.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        input.extend_from_slice(&prefix);
        input.extend_from_slice(&bytes);
        let expected_offset = if kind == 0 && bytes.is_empty() {
            0
        } else {
            let offset = model.len();
            if kind == 1 {
                model.extend_from_slice(&prefix);
            }
            model.extend_from_slice(&bytes);
            model.push(0);
            offset
        };
        let offset = if kind == 0 {
            table.add(&bytes)
        } else {
            table.add_prefix(&prefix, &bytes)
        }
        .unwrap();
        assert_eq!(offset, expected_offset, "operation {n}");
        assert_eq!(table.data(), model, "operation {n}");
        assert_eq!(table.size(), model.len(), "operation {n}");
        record(&mut expected, offset, table.data());
    }
    oracle::compare_bytes(&oracle, &work, &input, &expected, "string-table operations");
    eprintln!(
        "TPDE_STRING_TABLE_ORACLE PASS operations=512 snapshots=513 bytes={}",
        expected.len()
    );
    fs::remove_dir_all(work).unwrap();
}

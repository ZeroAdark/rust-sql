// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause
#![cfg(feature = "upstream-oracle")]

mod common;
mod decoder_runtime;
mod format_runtime;
mod helper_runtime;

#[allow(unused_parens)]
mod generated_dispatch {
    include!(concat!(env!("OUT_DIR"), "/runtime-encoder-dispatch.rs"));
}

use std::fs;

#[test]
fn every_generated_encoder_calls_the_real_runtime_and_matches_c() {
    let dir = common::prepare("encoders");
    let references = common::compile_all(&dir, "encoder_reference.cpp");
    let counts = common::counts(&dir.join("encoder-counts.tsv"));
    assert!(counts["forms"] >= 3000);
    assert!(counts["helper_forms"] >= 150);
    if tpde_disarm::GENERATION_PROFILE == "really-all" {
        assert_eq!(counts["forms"], 3911);
        assert_eq!(counts["helper_forms"], 217);
    }
    assert!(counts["cases"] >= counts["forms"]);
    assert_eq!(counts["record_bytes"], 68);
    let names = fs::read_to_string(dir.join("encoder-names.txt")).unwrap();
    assert_eq!(
        names.lines().collect::<Vec<_>>(),
        generated_dispatch::ENCODER_NAMES
    );
    let input = fs::read(dir.join("encoder-input.bin")).unwrap();
    assert_eq!(input.len(), counts["cases"] * 68);
    let mut output = Vec::with_capacity(counts["cases"] * 4);
    let mut accepted = vec![0usize; counts["forms"]];
    let mut rejected = vec![0usize; counts["forms"]];
    for record in input.chunks_exact(68) {
        let id = u32::from_le_bytes(record[..4].try_into().unwrap()) as usize;
        let mut args = [0; 8];
        for (arg, bytes) in args.iter_mut().zip(record[4..].chunks_exact(8)) {
            *arg = u64::from_le_bytes(bytes.try_into().unwrap());
        }
        let encoded = generated_dispatch::encode(id, &args);
        if encoded == 0 {
            rejected[id] += 1;
        } else {
            accepted[id] += 1;
        }
        output.extend_from_slice(&encoded.to_le_bytes());
    }
    let mut outcomes = String::from("encoder\tname\taccepted\trejected\n");
    for (id, name) in generated_dispatch::ENCODER_NAMES.iter().enumerate() {
        assert!(accepted[id] + rejected[id] > 0, "unexecuted encoder {name}");
        outcomes.push_str(&format!(
            "{id}\t{name}\t{}\t{}\n",
            accepted[id], rejected[id]
        ));
    }
    assert!(accepted.iter().sum::<usize>() > 0 && rejected.iter().sum::<usize>() > 0);
    fs::write(dir.join("encoder-outcomes.tsv"), outcomes).unwrap();
    fs::write(dir.join("encoder-rust.bin"), &output).unwrap();
    let modes = common::compare_all(
        &dir,
        &references,
        &input,
        &output,
        "all generated encoders with actual handwritten helpers",
    );
    let uncovered = generated_dispatch::ENCODER_NAMES
        .iter()
        .zip(&accepted)
        .filter_map(|(name, &count)| (count == 0).then_some(*name))
        .collect::<Vec<_>>();
    assert!(
        uncovered.is_empty(),
        "encoders without a nonzero encoding witness: {}",
        uncovered.join(", ")
    );
    println!(
        "TPDE_DISARM_RUNTIME_ENCODERS_OK profile={} forms={} helper_forms={} cases={} cpp_modes={modes}",
        tpde_disarm::GENERATION_PROFILE, counts["forms"], counts["helper_forms"], counts["cases"]
    );
}

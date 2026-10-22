// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::{env, fs, path::PathBuf};
use tpde_disarm_gen::{generate, Options};

#[path = "tests/dispatch_build.rs"]
mod dispatch_build;

fn main() {
    println!("cargo:rerun-if-changed=reference/desc.txt");
    println!("cargo:rerun-if-changed=reference/feat.txt");
    let desc = fs::read_to_string("reference/desc.txt").expect("pinned Disarm instructions");
    let features = fs::read_to_string("reference/feat.txt").expect("pinned Disarm features");
    let profile = if env::var_os("CARGO_FEATURE_REALLY_ALL").is_some() {
        "really-all"
    } else {
        "all"
    };
    println!("cargo:rustc-env=TPDE_DISARM_PROFILE={profile}");
    let generated = generate(
        &[&desc],
        Some(&features),
        &Options {
            features: Some(profile.into()),
            coverage: false,
            encode_in_header: true,
        },
    )
    .expect("generate complete Disarm runtime");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    if env::var_os("CARGO_FEATURE_UPSTREAM_ORACLE").is_some() {
        fs::write(
            out.join("runtime-encoder-dispatch.rs"),
            dispatch_build::generate(&generated.public_rust, &generated.private_rust),
        )
        .expect("write test-only encoder dispatch");
    }
    fs::write(out.join("public.rs"), generated.public_rust).expect("write public runtime");
    fs::write(out.join("private.rs"), generated.private_rust).expect("write private runtime");
    fs::write(out.join("forms.tsv"), generated.manifest).expect("write form manifest");
}

use gravity_lint::*;

#[test]
fn kernels_depend_on_the_abi_and_the_program_layouts_only() {
    let deps = cargo_dependencies(&gravity_root().join(KERNEL_CRATE));
    println!("gravity_kernels [dependencies]: {}", deps.join(", "));
    let v = kernel_boundary_violations();
    assert!(v.is_empty(), "kernel boundary (docs/gravity/abi-v2.md §0):\n{}", v.join("\n"));
    assert!(deps.contains(&"gravity_abi".to_owned()));
}

#[test]
fn the_boundary_names_are_the_gravity_crates() {
    let dirs: Vec<String> = crate_dirs().iter().map(|d| d.file_name().unwrap().to_string_lossy().into_owned()).collect();
    for name in KERNEL_FORBIDDEN_PATHS {
        let dir = name.trim_start_matches("gravity_");
        assert!(dirs.contains(&dir.to_owned()), "{name} is not a Gravity crate");
    }
}

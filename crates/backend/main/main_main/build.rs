// Stage initdb's share inputs and every contrib and procedural-language
// crate's extension/ files into
// target/<profile>/share/extension/ beside the postgres binary — no install
// step exists, and CREATE EXTENSION resolves this staged dir first (see
// commands/extension control.rs staged_extension_dir). Dist binaries fetched
// from S3 travel without the staged dir; PGRUST_PGSHAREDIR-resolved system
// files cover that path.
use std::path::{Path, PathBuf};

/// D4503: link the x86_64 Linux server binary with -z pack-relative-relocs (DT_RELR). THE switch, DORMANT (false): nothing
/// else depends on it. Flip it to true ONLY after both:
/// (1) an x86_64 test of a binary linked with it: readelf shows DT_RELR and the size drop, the server starts, and a short
///     regress / RTT smoke passes (the x86 link is untested: no x86_64 box was available when this landed);
/// (2) the user's ruling on the release tarball's glibc floor (D4516): DT_RELR needs glibc >= 2.36, so a 2.34 floor rules it out.
///
/// What it buys: the binary's ~134k R_*_RELATIVE relocations (vtables, fmgr and seam tables, static string slices) take 24 bytes
/// each in .rela.dyn; DT_RELR packs them into ~45 KB of bitmaps and the dynamic loader applies them faster at every exec. What it
/// costs: a GLIBC_ABI_DT_RELR version need (glibc >= 2.36). Today's shipped builds would not move: the release tarball builds on
/// Ubuntu 24.04 and already needs GLIBC_2.38; the Docker image builds and runs on Debian 12, glibc 2.36.
///
/// x86_64 only, because every linker that links it there packs it (rust-lld, rustc's default on x86_64-unknown-linux-gnu;
/// mold; GNU ld >= 2.38). NOT aarch64: GNU ld, which links every shipped aarch64 artifact (release tarball, benchmark
/// rigs, PGO builds, Docker), ignores the option there ("warning: -z pack-relative-relocs ignored", GNU ld 2.42), while
/// the boxes' mold packs it (-4.0 MB on the release bin): enabling it would make box-built Graviton binaries differ
/// from the shipped ones. Revisit if the aarch64 release link moves to mold or lld.
const PACK_RELATIVE_RELOCS_X86_64: bool = false;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    for sub in ["contrib", "pl", "bin/initdb/share"] {
        let dir = manifest_dir.join("../../..").join(sub).canonicalize().unwrap();
        println!("cargo:rerun-if-changed={}", dir.display());
    }

    // OUT_DIR = target/<profile>/build/main_main-<hash>/out
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR has the standard cargo build layout");

    // on wasm32 the memory stops 1 MiB below 4 GiB (65520 pages): the top of the address space carries the
    // statuses a host answers as a pointer back to the program (gravity_abi host::words STATUS_WINDOW), so no allocation
    // may lie there. Every build of the server binary takes it, whichever script runs cargo.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-bins=--max-memory=4293918720");
    }
    // D4503: DT_RELR on the x86_64 Linux server binary, dormant; see PACK_RELATIVE_RELOCS_X86_64.
    let target_is = |var: &str, want: &str| std::env::var(var).as_deref() == Ok(want);
    let linux_gnu = target_is("CARGO_CFG_TARGET_OS", "linux") && target_is("CARGO_CFG_TARGET_ENV", "gnu");
    if PACK_RELATIVE_RELOCS_X86_64 && linux_gnu && target_is("CARGO_CFG_TARGET_ARCH", "x86_64") {
        println!("cargo:rustc-link-arg-bin=postgres=-Wl,-z,pack-relative-relocs");
    }

    println!("cargo:rerun-if-env-changed=PGRUST_TEST_MODULES");
    let test_modules = std::env::var("PGRUST_TEST_MODULES").as_deref() == Ok("1");
    stage_share(&manifest_dir, &profile_dir.join("share"), test_modules);

    stamp_build_id(&manifest_dir);
}

/// lib.rs PGRUST_BUILD_ID: $PGRUST_BUILD_ID if set ("" = none), else the checkout's short HEAD. Stamped here because
/// nothing depends on main_main: a commit recompiles this crate only, not guc_tables' hundreds of dependents.
fn stamp_build_id(manifest_dir: &Path) {
    println!("cargo:rerun-if-env-changed=PGRUST_BUILD_ID");
    let id = match std::env::var("PGRUST_BUILD_ID") {
        Ok(v) => v.trim().to_owned(),
        Err(_) => checkout_head(manifest_dir).unwrap_or_default(),
    };
    assert!(
        id.len() <= 40 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b)),
        "PGRUST_BUILD_ID must be at most 40 characters of [0-9A-Za-z._+-], got {id:?}"
    );
    println!("cargo:rustc-env=PGRUST_BUILD_ID={id}");
}

/// None outside a git checkout that tracks this tree (e.g. a tarball unpacked inside another repository).
fn checkout_head(manifest_dir: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").args(args).current_dir(manifest_dir).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let v = String::from_utf8(out.stdout).ok()?.trim().to_owned();
        (!v.is_empty()).then_some(v)
    };
    git(&["ls-files", "--error-unmatch", "Cargo.toml"])?;
    let mut watch = vec![("HEAD".to_owned(), true)];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push((branch, true)); // a packed ref: its nearest directory sees the next loose ref
    }
    watch.push(("packed-refs".to_owned(), false));
    for (name, nearest_dir) in watch {
        let Some(path) = git(&["rev-parse", "--git-path", &name]) else { continue };
        let path = manifest_dir.join(path);
        let found = match nearest_dir {
            true => path.ancestors().find(|p| p.exists()),
            false => path.exists().then_some(path.as_path()),
        };
        if let Some(p) = found {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
    git(&["rev-parse", "--short=7", "HEAD"])
}

fn stage_share(manifest_dir: &Path, share: &Path, test_modules: bool) {
    let crates_dir = manifest_dir.join("../../..");
    let staged = share.join("extension");
    std::fs::create_dir_all(&staged).unwrap();

    // Contrib text search data files (unaccent.rules, ...) stage the same way
    // into share/tsearch_data/; ts_locale's get_tsearch_config_filename
    // resolves the staged file first.
    let staged_tsdata = share.join("tsearch_data");
    std::fs::create_dir_all(&staged_tsdata).unwrap();

    // initdb's share inputs stage into share/ itself (crates/bin/initdb/share).
    stage_tree(&crates_dir.join("bin/initdb/share"), share);

    for sub in ["contrib", "pl"] {
        stage_contrib(&crates_dir.join(sub), &staged, &staged_tsdata, test_modules);
    }
}

fn stage_contrib(contrib_dir: &Path, staged: &Path, staged_tsdata: &Path, test_modules: bool) {
    for crate_entry in std::fs::read_dir(contrib_dir).unwrap() {
        let crate_path = crate_entry.unwrap().path();
        for (sub, staged) in [("extension", staged), ("tsearch_data", staged_tsdata)] {
            let src_dir = crate_path.join(sub);
            if !src_dir.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(&src_dir).unwrap() {
                let src = f.unwrap().path();
                if !src.is_file() {
                    continue;
                }
                let dst = staged.join(src.file_name().unwrap());
                let is_test_module = matches!(
                    crate_path.file_name().and_then(|name| name.to_str()),
                    Some("injection_points" | "test_custom_types")
                );
                if is_test_module && !test_modules {
                    if dst.exists() {
                        std::fs::remove_file(&dst).unwrap();
                    }
                } else {
                    copy_if_changed(&src, &dst);
                }
            }
        }
    }
}

fn stage_tree(src_dir: &Path, dst_dir: &Path) {
    std::fs::create_dir_all(dst_dir).unwrap();
    for f in std::fs::read_dir(src_dir).unwrap() {
        let src = f.unwrap().path();
        let dst = dst_dir.join(src.file_name().unwrap());
        if src.is_dir() {
            stage_tree(&src, &dst);
        } else if src.is_file() {
            copy_if_changed(&src, &dst);
        }
    }
}

// Unconditional copies would dirty mtimes and cascade rebuilds.
fn copy_if_changed(src: &Path, dst: &Path) {
    let same = match (std::fs::read(src), std::fs::read(dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        std::fs::copy(src, dst).unwrap();
    }
}

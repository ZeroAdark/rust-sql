// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub struct Reference {
    pub name: &'static str,
    pub assertions: bool,
    pub executable: PathBuf,
}

pub fn work_directory() -> PathBuf {
    let base = env::var_os("COMPONENT_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let work = base.join(format!(
        "tpde-compiler-a64-raw-{}-{}",
        if cfg!(tpde_address_sanitizer) {
            "asan"
        } else {
            "native"
        },
        std::process::id()
    ));
    fs::create_dir_all(&work).expect("create A64 compiler raw oracle artifact directory");
    work
}

fn recorded(command: &mut Command, work: &Path, name: &str) -> Output {
    fs::write(
        work.join(format!("{name}.command.txt")),
        format!("{command:?}\n"),
    )
    .expect("record oracle command");
    let result = command.output().unwrap_or_else(|error| {
        fs::write(
            work.join(format!("{name}.launch-error.txt")),
            error.to_string(),
        )
        .unwrap();
        panic!("could not launch {name}: {error}")
    });
    fs::write(work.join(format!("{name}.stdout")), &result.stdout).unwrap();
    fs::write(work.join(format!("{name}.stderr")), &result.stderr).unwrap();
    fs::write(
        work.join(format!("{name}.status.txt")),
        result.status.to_string(),
    )
    .unwrap();
    assert!(
        result.status.success(),
        "{name} failed with {}: {} (artifacts: {})",
        result.status,
        String::from_utf8_lossy(&result.stderr),
        work.display()
    );
    result
}

const VERIFY_REFERENCE: &str = r#"
import hashlib, json, os, pathlib, shutil, sys
def require(condition, message):
    if not condition:
        raise RuntimeError(message)
root = pathlib.Path(sys.argv[1])
work = pathlib.Path(sys.argv[2])
records = []
for relative, revision, repository, count in [
    (root, '116b2b83eb8491fd63c017285c91b007b8a26a67', 'https://github.com/tpde2/tpde', 1),
    (root.parents[2] / 'compiler/tests/reference', '116b2b83eb8491fd63c017285c91b007b8a26a67', 'https://github.com/tpde2/tpde', 39),
    (root.parents[2] / 'function-writer-a64/tests/upstream', '116b2b83eb8491fd63c017285c91b007b8a26a67', 'https://github.com/tpde2/tpde', 24),
    (root.parents[2] / 'function-writer-a64/tests/upstream/disarm', '2d13d3f410a52daff1c5d8ef07d623332f372560', 'https://github.com/aengelke/disarm', 7),
]:
    source = pathlib.Path(relative)
    manifest = json.loads((source / 'manifest.json').read_text())
    require(manifest['revision'] == revision, 'wrong pinned revision: ' + str(relative))
    require(manifest['repository'] == repository, (relative, 'repository'))
    require(len(manifest['files']) == count, (relative, 'source count'))
    seen = set()
    for entry in manifest['files']:
        name = entry['path']
        require(name not in seen, (relative, name, 'duplicate source'))
        seen.add(name)
        path = source / name
        require(path.resolve().is_relative_to(source.resolve()), (relative, name))
        data = path.read_bytes()
        sha256 = hashlib.sha256(data).hexdigest()
        git_blob = hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
        require(sha256 == entry['sha256'], (relative, name, 'sha256'))
        require(git_blob == entry['git_blob'], (relative, name, 'git blob'))
        records.append(dict(repository=repository, revision=revision, path=str(path),
                            sha256=sha256, git_blob=git_blob, bytes=len(data)))
harness = []
harness_paths = [root.parent / 'raw_oracle.cpp', *root.parent.rglob('*.rs'),
                 root.parent.parent / 'Cargo.toml',
                 root.parent.parent.parent / 'test-support' / 'oracle.rs']
for path in sorted(harness_paths):
    data = path.read_bytes()
    harness.append(dict(path=str(path), sha256=hashlib.sha256(data).hexdigest(), bytes=len(data)))
def compiler_identity(command):
    resolved = shutil.which(command)
    require(resolved is not None, ('compiler not found', command))
    path = pathlib.Path(resolved).resolve()
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return dict(command=command, resolved=str(path), sha256=digest.hexdigest())
cxx = os.environ['CXX']
cc = os.environ.get('CC', cxx)
configuration = dict(cxx=compiler_identity(cxx), cc=compiler_identity(cc),
    c_standard='c11', cpp_standard='c++20', optimization='-O3', stdlib='libc++',
    modes=[dict(name='assert', assertions=True, sanitized=False),
           dict(name='ndebug', assertions=False, sanitized=False),
           dict(name='asan-ubsan', assertions=True, sanitized=True)],
    sanitizer_flags=['-fsanitize=address,undefined', '-fno-sanitize-recover=all',
                     '-fno-omit-frame-pointer'],
    python=sys.version)
identity = dict(verification='PASS', reference_files=records, harness_files=harness,
                build_configuration=configuration,
                disarm_features='all', disarm_encode_in_header=True)
(work / 'reference-source-identity.json').write_text(json.dumps(identity, indent=2) + '\n')
print('TPDE_COMPILER_A64_RAW_REFERENCE_SOURCES PASS tpde_manifest_entries=64 disarm=7')
"#;

fn build_references(work: &Path) -> Vec<Reference> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let reference = root.join("tests/raw_reference");
    let compiler_reference = root.join("../compiler/tests/reference");
    let writer_reference = root.join("../function-writer-a64/tests/upstream");
    let disarm = writer_reference.join("disarm");
    let compiler_include = compiler_reference.join("tpde/include");
    let writer_include = writer_reference.join("tpde/include");
    let include = reference.join("tpde/include");
    let generated = work.join("disarm-generated");
    fs::create_dir_all(&generated).unwrap();
    recorded(
        Command::new("python3")
            .args(["-I", "-c", VERIFY_REFERENCE])
            .arg(&reference)
            .arg(work),
        work,
        "verify-reference",
    );
    recorded(
        Command::new("python3")
            .arg("-I")
            .arg(disarm.join("parse.py"))
            .arg(generated.join("disarm64-public.inc"))
            .arg(generated.join("disarm64-private.inc"))
            .arg(disarm.join("desc.txt"))
            .args(["--features", "all", "--feature-desc"])
            .arg(disarm.join("feat.txt"))
            .arg("--encode-in-header"),
        work,
        "generate-disarm",
    );
    let cxx = env::var("CXX").expect("the runner profile must configure CXX");
    let cc = env::var("CC").unwrap_or_else(|_| cxx.clone());
    recorded(Command::new(&cxx).arg("--version"), work, "cxx-version");
    recorded(Command::new(&cc).arg("--version"), work, "cc-version");
    let mut identity = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    identity.arg("-vV");
    recorded(&mut identity, work, "rustc-version");
    let mut references = Vec::new();
    for (name, assertions, sanitized) in [
        ("assert", true, false),
        ("ndebug", false, false),
        ("asan-ubsan", true, true),
    ] {
        let object = work.join(format!("disarm-encode-{name}.o"));
        let mut c_command = Command::new(&cc);
        c_command
            .args(["-x", "c", "-std=c11", "-O3"])
            .arg("-I")
            .arg(&disarm)
            .arg("-I")
            .arg(&generated);
        if !assertions {
            c_command.arg("-DNDEBUG");
        }
        if sanitized {
            c_command.args([
                "-fsanitize=address,undefined",
                "-fno-sanitize-recover=all",
                "-fno-omit-frame-pointer",
            ]);
        }
        c_command
            .arg("-c")
            .arg(disarm.join("encode.c"))
            .arg("-o")
            .arg(&object);
        recorded(&mut c_command, work, &format!("compile-disarm-{name}"));
        let executable = work.join(format!("reference-{name}"));
        let mut sources = vec![root.join("tests/raw_oracle.cpp")];
        sources.extend(
            [
                "tpde/src/FunctionWriter.cpp",
                "tpde/src/Assembler.cpp",
                "tpde/src/AssemblerElf.cpp",
                "tpde/src/StringTable.cpp",
                "tpde/src/ValueAssignment.cpp",
                "tpde/src/test/TestIR.cpp",
                "tpde/src/base.cpp",
                "tpde/src/util/SmallVector.cpp",
            ]
            .map(|source| compiler_reference.join(source)),
        );
        sources.push(writer_reference.join("tpde/src/arm64/FunctionWriterA64.cpp"));
        sources.push(object);
        let mut flags = vec![
            "-stdlib=libc++",
            "-I",
            compiler_reference.to_str().unwrap(),
            "-I",
            compiler_include.to_str().unwrap(),
            "-I",
            writer_include.to_str().unwrap(),
            "-I",
            disarm.to_str().unwrap(),
            "-I",
            generated.to_str().unwrap(),
        ];
        if sanitized {
            flags.extend([
                "-fsanitize=address,undefined",
                "-fno-sanitize-recover=all",
                "-fno-omit-frame-pointer",
            ]);
        }
        let mut cpp_command = Command::new(&cxx);
        cpp_command.args(["-std=c++20", "-O3"]);
        if !assertions {
            cpp_command.arg("-DNDEBUG");
        }
        cpp_command
            .arg("-I")
            .arg(&include)
            .args(&flags)
            .args(&sources)
            .arg("-o")
            .arg(&executable);
        fs::write(
            work.join(format!("compile-cpp-{name}.command.txt")),
            format!("{cpp_command:?}\n"),
        )
        .unwrap();
        let result = std::panic::catch_unwind(|| {
            oracle::compile_cpp_with_flags(&include, &executable, &sources, assertions, &flags);
        });
        if let Err(failure) = result {
            let message = failure
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| failure.downcast_ref::<&str>().copied())
                .unwrap_or("C++ compiler helper panicked without a string payload");
            fs::write(
                work.join(format!("compile-cpp-{name}.failure.txt")),
                message,
            )
            .unwrap();
            std::panic::resume_unwind(failure);
        }
        fs::write(
            work.join(format!("compile-cpp-{name}.status.txt")),
            "PASS; shared helper retains compiler stdout/stderr under oracle-processes\n",
        )
        .unwrap();
        references.push(Reference {
            name,
            assertions,
            executable,
        });
    }
    references
}

const RECEIPT: &str = r#"
import hashlib, json, os, pathlib, sys
def require(condition, message):
    if not condition:
        raise RuntimeError(message)
mode = sys.argv[1]
shared = pathlib.Path(sys.argv[2])
require(shared.is_dir() and not shared.is_symlink(), 'shared reference directory missing or replaced')
root = shared.resolve()
current = pathlib.Path(sys.argv[3])
receipt_path = root / 'receipt.json'
modes = [dict(name='assert', assertions=True, executable='reference-assert'),
         dict(name='ndebug', assertions=False, executable='reference-ndebug'),
         dict(name='asan-ubsan', assertions=True, executable='reference-asan-ubsan')]
required = {'reference-source-identity.json',
            'disarm-generated/disarm64-public.inc',
            'disarm-generated/disarm64-private.inc'}
for compiler in ['cc', 'cxx', 'rustc']:
    required.add(compiler + '-version.stdout')
for reference in modes:
    name = reference['name']
    required.update([reference['executable'], 'disarm-encode-' + name + '.o',
        'compile-disarm-' + name + '.command.txt',
        'compile-disarm-' + name + '.status.txt',
        'compile-cpp-' + name + '.command.txt',
        'compile-cpp-' + name + '.status.txt'])
def artifact_path(relative):
    require(isinstance(relative, str), 'artifact path must be a string')
    path = root / relative
    require(not pathlib.Path(relative).is_absolute(), relative)
    require(path.resolve().is_relative_to(root), ('artifact escapes shared directory', relative))
    require(path.is_file(), ('missing artifact', relative))
    return path

def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()

def validate(receipt):
    require(receipt['schema'] == 1, 'receipt schema')
    require(receipt['modes'] == modes, 'reference modes')
    identity = json.loads((current / 'reference-source-identity.json').read_text())
    require(receipt['source_identity'] == identity, 'source or compiler configuration changed')
    names = set()
    for record in receipt['artifacts']:
        relative = record['path']
        require(relative not in names, ('duplicate artifact', relative))
        names.add(relative)
        path = artifact_path(relative)
        require(path.stat().st_size == record['bytes'], ('artifact size changed', relative))
        require(digest(path) == record['sha256'], ('artifact hash changed', relative))
    require(required <= names, ('missing required artifacts', sorted(required - names)))
    for reference in modes:
        require(os.access(artifact_path(reference['executable']), os.X_OK), 'reference is not executable')
    return names

if mode == 'publish':
    require(not receipt_path.exists(), 'reference receipt already exists')
    records = []
    for path in sorted(root.rglob('*')):
        if not path.is_file() or path.name.startswith('publish-receipt.'):
            continue
        relative = str(path.relative_to(root))
        records.append(dict(path=relative, bytes=path.stat().st_size, sha256=digest(path)))
    receipt = dict(schema=1, modes=modes, artifacts=records,
                   source_identity=json.loads((current / 'reference-source-identity.json').read_text()))
    validate(receipt)
    temporary = root / ('receipt.json.pending-' + str(os.getpid()))
    with temporary.open('x') as stream:
        json.dump(receipt, stream, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, receipt_path)
    print('TPDE_COMPILER_A64_RAW_REFERENCE_RECEIPT PASS cpp_modes=3')
elif mode == 'require':
    require(receipt_path.is_file(), 'mandatory reference preparation has not completed')
    receipt = json.loads(receipt_path.read_text())
    names = validate(receipt)
    report = dict(verification='PASS', receipt=str(receipt_path), receipt_sha256=digest(receipt_path),
                  source_identity='MATCH', build_configuration='MATCH', artifacts=len(names), modes=modes)
    (current / 'reference-receipt-verification.json').write_text(json.dumps(report, indent=2) + '\n')
    print('TPDE_COMPILER_A64_RAW_REFERENCE_REUSE PASS cpp_modes=3')
else:
    raise ValueError('unknown receipt mode: ' + mode)
"#;

fn shared_root() -> PathBuf {
    let root = PathBuf::from(
        env::var_os("COMPONENT_ARTIFACT_DIR")
            .expect("reference preparation requires COMPONENT_ARTIFACT_DIR"),
    );
    root.canonicalize()
        .expect("resolve component artifact root")
        .join("a64-compiler-raw-reference")
}

pub fn prepare_reference() -> PathBuf {
    let work = shared_root();
    fs::create_dir(&work).expect("A64 references must be prepared exactly once per qualification");
    let references = build_references(&work);
    assert_eq!(references.len(), 3);
    recorded(
        Command::new("python3")
            .args(["-I", "-c", RECEIPT, "publish"])
            .arg(&work)
            .arg(&work),
        &work,
        "publish-receipt",
    );
    let controls = work_directory();
    let command_artifact = work.join("compile-cpp-assert.command.txt");
    let original = fs::read(&command_artifact).unwrap();
    let mut changed = original.clone();
    changed.push(b' ');
    fs::write(&command_artifact, changed).unwrap();
    let result = Command::new("python3")
        .args(["-I", "-O", "-c", RECEIPT, "require"])
        .arg(&work)
        .arg(&work)
        .output();
    fs::write(&command_artifact, original).unwrap();
    assert_refusal(
        result.unwrap(),
        &controls,
        "optimized-artifact-negative",
        "RuntimeError: ('artifact size changed', 'compile-cpp-assert.command.txt')",
    );
    recorded(
        Command::new("python3")
            .args(["-I", "-O", "-c", RECEIPT, "require"])
            .arg(&work)
            .arg(&work),
        &controls,
        "verify-artifact-restored",
    );
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/raw_reference");
    let manifest = source.join("manifest.json");
    let original = fs::read_to_string(&manifest).unwrap();
    let changed = original.replacen(
        "116b2b83eb8491fd63c017285c91b007b8a26a67",
        "0000000000000000000000000000000000000000",
        1,
    );
    assert_ne!(changed, original);
    fs::write(&manifest, changed).unwrap();
    let result = Command::new("python3")
        .args(["-I", "-O", "-c", VERIFY_REFERENCE])
        .arg(&source)
        .arg(&controls)
        .output();
    fs::write(&manifest, original).unwrap();
    assert_refusal(
        result.unwrap(),
        &controls,
        "optimized-pin-negative",
        &format!("RuntimeError: wrong pinned revision: {}", source.display()),
    );
    let restored = require_references(&controls);
    assert_eq!(restored.len(), 3);
    work.join("receipt.json")
}

fn assert_refusal(output: Output, work: &Path, label: &str, expected: &str) {
    fs::write(work.join(format!("{label}.stdout")), &output.stdout).unwrap();
    fs::write(work.join(format!("{label}.stderr")), &output.stderr).unwrap();
    fs::write(
        work.join(format!("{label}.status.txt")),
        output.status.to_string(),
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(1), "{label}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line == expected),
        "{label}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn require_references(work: &Path) -> Vec<Reference> {
    let shared = shared_root();
    let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/raw_reference");
    recorded(
        Command::new("python3")
            .args(["-I", "-c", VERIFY_REFERENCE])
            .arg(&reference)
            .arg(work),
        work,
        "verify-reference",
    );
    recorded(
        Command::new("python3")
            .args(["-I", "-c", RECEIPT, "require"])
            .arg(&shared)
            .arg(work),
        work,
        "require-reference-receipt",
    );
    [("assert", true), ("ndebug", false), ("asan-ubsan", true)]
        .map(|(name, assertions)| Reference {
            name,
            assertions,
            executable: shared.join(format!("reference-{name}")),
        })
        .into()
}

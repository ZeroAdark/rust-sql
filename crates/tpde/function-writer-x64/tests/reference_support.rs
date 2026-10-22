// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "../../test-support/oracle.rs"]
mod oracle;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const RECEIPT: &str = r#"
import hashlib,json,os,pathlib,shutil,sys
root=pathlib.Path(sys.argv[1]).resolve()
work=pathlib.Path(sys.argv[2]).resolve()
action=sys.argv[3]
receipt=work/'prepared.json'
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()
source_names=['Cargo.toml','tests/upstream.rs','tests/oracle.cpp','tests/reference_prepare.rs','tests/reference_support.rs','../test-support/oracle.rs']
runner=pathlib.Path(os.environ['TPDE_RUNNER_DIR']).resolve()
source_names+=[os.path.relpath(runner/'units/tpde_function_writer_x64.json',root),os.path.relpath(runner/'profiles/tpde-component.json',root)]
for area,revision,count in [('tpde','116b2b83eb8491fd63c017285c91b007b8a26a67',26),('fadec','3994d89500985491b1a7ccc17827a22058b3de49',6)]:
    manifest=f'tests/reference/{area}/manifest.json'
    source_names.append(manifest)
    parsed=json.loads((root/manifest).read_text())
    assert parsed['revision']==revision and len(parsed['files'])==count
    for entry in parsed['files']:
        name=f'tests/reference/{area}/'+entry['path']
        assert digest(root/name)==entry['sha256'], f'pinned source changed: {name}'
        source_names.append(name)
assert len(source_names)==len(set(source_names))
compiler_name=os.environ.get('CXX','clang++')
compiler=pathlib.Path(shutil.which(compiler_name) or compiler_name).resolve(strict=True)
python=pathlib.Path(sys.executable).resolve(strict=True)
configuration={
    'format':1,
    'compiler':{'argument':compiler_name,'path':str(compiler),'sha256':digest(compiler)},
    'python':{'path':str(python),'version':sys.version,'sha256':digest(python)},
    'encoder_profile':['encode2','--32','--64'],
    'cpp_modes':['assertions','ndebug','sanitizers'],
    'encoder_modes':['fadec-c11','fadec-sanitizers'],
    'sources':{name:digest(root/name) for name in sorted(source_names)},
}
output_names=['generated/fadec-encode2-public.inc','generated/fadec-encode2-private.inc','fadec-c11.o','fadec-sanitizers.o','assertions','ndebug','sanitizers']
for name in ['generate-fadec','fadec-c11','fadec-sanitizers']:
    output_names.extend(name+suffix for suffix in ['.command.txt','.stdout','.stderr'])
output_names.extend(name+'.command.txt' for name in configuration['cpp_modes'])
assert len(output_names)==len(set(output_names))
def verify(path):
    data=json.loads(path.read_text())
    assert set(data)=={'configuration','outputs'}, 'wrong receipt fields'
    assert data['configuration']==configuration, 'prepared source/compiler/configuration changed'
    assert set(data['outputs'])==set(output_names), 'wrong prepared artifact roster'
    for name in output_names:
        path=work/name
        assert path.is_file() and digest(path)==data['outputs'][name], f'prepared artifact changed: {name}'
    for name in ['assertions','ndebug','sanitizers']:
        assert (work/name).stat().st_size and os.access(work/name,os.X_OK)
    return data
if action=='publish':
    assert not receipt.exists(), 'readiness must be invalidated before compilation'
    candidate=work/'prepared.pending.json'
    candidate.write_text(json.dumps({'configuration':configuration,'outputs':{name:digest(work/name) for name in output_names}},indent=2)+'\n')
    try:
        verify(receipt)
    except FileNotFoundError:
        pass
    else:
        raise AssertionError('missing preparation receipt was accepted')
    verify(candidate)
    changed=work/'generated/fadec-encode2-public.inc'
    original=changed.stat().st_size
    try:
        with changed.open('ab') as output:
            output.write(b'\n')
        try:
            verify(candidate)
        except AssertionError:
            pass
        else:
            raise AssertionError('stale prepared artifact was accepted')
    finally:
        with changed.open('r+b') as output:
            output.truncate(original)
    verify(candidate)
    candidate.replace(receipt)
elif action=='verify':
    verify(receipt)
else:
    raise AssertionError('unknown preparation action')
print('TPDE_X64_WRITER_PREPARED_RECEIPT PASS action='+action+' sources='+str(len(source_names))+' outputs='+str(len(output_names)))
"#;

fn prepared_directory() -> PathBuf {
    PathBuf::from(
        std::env::var_os("COMPONENT_ARTIFACT_DIR")
            .expect("reference preparation requires COMPONENT_ARTIFACT_DIR"),
    )
    .join("x64-writer-reference")
}

fn receipt(root: &Path, work: &Path, action: &str, log: &Path) {
    checked(
        Command::new("python3")
            .args(["-I", "-B", "-c", RECEIPT])
            .arg(root)
            .arg(work)
            .arg(action),
        log,
        &format!("reference-{action}"),
    );
}

pub fn prepare(root: &Path) {
    let work = prepared_directory();
    fs::create_dir_all(&work).unwrap();
    let ready = work.join("prepared.json");
    if ready.exists() {
        fs::remove_file(ready).unwrap();
    }
    let executables = compile_references(root, &work);
    assert_eq!(executables.len(), 3);
    receipt(root, &work, "publish", &work);
}

pub fn load_prepared(root: &Path, log: &Path) -> Vec<PathBuf> {
    let work = prepared_directory();
    receipt(root, &work, "verify", log);
    ["assertions", "ndebug", "sanitizers"]
        .into_iter()
        .map(|name| work.join(name))
        .collect()
}

pub fn checked(command: &mut Command, work: &Path, name: &str) {
    fs::write(
        work.join(format!("{name}.command.txt")),
        format!("{command:?}\n"),
    )
    .unwrap();
    let result = command.output().expect("start reference tool");
    fs::write(work.join(format!("{name}.stdout")), &result.stdout).unwrap();
    fs::write(work.join(format!("{name}.stderr")), &result.stderr).unwrap();
    assert!(
        result.status.success(),
        "{name}: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
}
fn compile_references(root: &Path, work: &Path) -> Vec<PathBuf> {
    let tpde = root.join("tests/reference/tpde");
    let fadec = root.join("tests/reference/fadec");
    let generated = work.join("generated");
    fs::create_dir_all(&generated).unwrap();
    checked(
        Command::new("python3")
            .arg("-I")
            .arg("-B")
            .arg(fadec.join("parseinstrs.py"))
            .arg("encode2")
            .arg(fadec.join("instrs.txt"))
            .arg(generated.join("fadec-encode2-public.inc"))
            .arg(generated.join("fadec-encode2-private.inc"))
            .args(["--32", "--64"]),
        work,
        "generate-fadec",
    );
    let compiler = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    let mut objects = vec![];
    for sanitized in [false, true] {
        let name = if sanitized {
            "fadec-sanitizers"
        } else {
            "fadec-c11"
        };
        let object = work.join(format!("{name}.o"));
        let mut command = Command::new(&compiler);
        if sanitized {
            command.args([
                "-x",
                "c++",
                "-std=c++20",
                "-stdlib=libc++",
                "-O1",
                "-fsanitize=address,undefined",
                "-fno-sanitize-recover=all",
                "-fno-omit-frame-pointer",
            ]);
        } else {
            command.args(["-x", "c", "-std=c11", "-O3"]);
        }
        command
            .arg("-I")
            .arg(&fadec)
            .arg("-I")
            .arg(&generated)
            .arg("-c")
            .arg(fadec.join("encode2.c"))
            .arg("-o")
            .arg(&object);
        checked(&mut command, work, name);
        objects.push(object);
    }
    let mut executables = vec![];
    for (name, assertions, sanitized) in [
        ("assertions", true, false),
        ("ndebug", false, false),
        ("sanitizers", true, true),
    ] {
        let mut sources = vec![
            root.join("tests/oracle.cpp"),
            tpde.join("tpde/src/x64/FunctionWriterX64.cpp"),
            tpde.join("tpde/src/FunctionWriter.cpp"),
            tpde.join("tpde/src/Assembler.cpp"),
            tpde.join("tpde/src/AssemblerElf.cpp"),
            tpde.join("tpde/src/StringTable.cpp"),
            tpde.join("tpde/src/util/SmallVector.cpp"),
            tpde.join("tpde/src/base.cpp"),
        ];
        sources.push(objects[usize::from(sanitized)].clone());
        let fadec_include = fadec.to_str().unwrap();
        let generated_include = generated.to_str().unwrap();
        let mut flags = vec![
            "-stdlib=libc++",
            "-fno-exceptions",
            "-I",
            fadec_include,
            "-I",
            generated_include,
        ];
        if sanitized {
            flags.extend([
                "-O1",
                "-fsanitize=address,undefined",
                "-fno-sanitize-recover=all",
                "-fno-omit-frame-pointer",
            ]);
        }
        let executable = work.join(name);
        let include = tpde.join("tpde/include");
        let mut command = Command::new(&compiler);
        command.args(["-std=c++20", "-O3"]);
        if !assertions {
            command.arg("-DNDEBUG");
        }
        command
            .arg("-I")
            .arg(&include)
            .args(&flags)
            .args(&sources)
            .arg("-o")
            .arg(&executable);
        fs::write(
            work.join(format!("{name}.command.txt")),
            format!("{command:?}\n"),
        )
        .unwrap();
        oracle::compile_cpp_with_flags(&include, &executable, &sources, assertions, &flags);
        executables.push(executable);
    }
    executables
}

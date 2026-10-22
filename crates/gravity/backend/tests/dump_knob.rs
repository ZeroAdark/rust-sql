//! The program dump (`gravity_backend::dump`, `pgrust.gravity_dump_dir`): a cache miss with the directory set writes
//! `<key>.ir`, `<key>.<arch>.bin` and `<key>.meta`; the `.ir` parses back to the program, the `.bin` is the mapped code
//! (a fresh uncached compile of the same program emits the same bytes), and `.meta` names each function's entry offset
//! into it. With the directory empty a miss writes nothing. The program is TH14-pgrc2's fold (D398's fixture).
#![cfg(target_arch = "aarch64")]

use gravity_ir::text::parse_program;
use gravity_ir::Func;
use std::sync::Mutex;

static DIR: Mutex<Option<String>> = Mutex::new(None);

fn dir() -> Option<String> {
    DIR.lock().unwrap().clone()
}

fn meta() -> String {
    "statement SELECT th14 fold (dump_knob test)\n".into()
}

fn program(tweak: bool) -> gravity_ir::Program {
    let text = include_str!("../tpde/tests/fixtures/th14_addhi_exhaustion.txt");
    // A different constant is a different content key: a second miss.
    let text = if tweak { text.replacen("(const i64 4611686018427387904)", "(const i64 4611686018427387905)", 1) } else { text.to_string() };
    parse_program(&text, &|_| Some(0x1000)).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn a_miss_writes_the_dump_and_an_empty_dir_writes_nothing() {
    gravity_backend::dump::install(gravity_backend::dump::Hooks { dir, meta });
    let out = std::env::temp_dir().join(format!("gravity-dump-knob-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    *DIR.lock().unwrap() = Some(out.display().to_string());
    let p = program(false);
    let (_, o) = gravity_backend::mapped_cached_all(&p, &[Func(0)]).unwrap_or_else(|e| panic!("{}", e.message()));
    let stem = format!("{:032x}", o.key.0);
    let ir = std::fs::read_to_string(out.join(format!("{stem}.ir"))).expect("the .ir");
    assert_eq!(parse_program(&ir, &|_| Some(0x1000)).unwrap(), p, "the .ir is the program");
    let bin = std::fs::read(out.join(format!("{stem}.aarch64.bin"))).expect("the .bin");
    let meta = std::fs::read_to_string(out.join(format!("{stem}.meta"))).expect("the .meta");
    assert!(meta.contains("arch aarch64\n") && meta.contains("backend tpde\n") && meta.contains("statement SELECT th14 fold"), "{meta}");
    assert!(meta.contains(&format!("code_bytes {}\n", bin.len())), "{meta}");
    // The bytes are the program's code: an uncached compile emits them again (bl immediates aside: absolute hosts).
    let ctx = mcx::MemoryContext::new("dump-knob");
    let c = gravity_tpde::compile(ctx.mcx(), &p).unwrap_or_else(|e| panic!("{}", e.message()));
    let r = c.mapper().get_mapped_range().unwrap();
    let fresh = unsafe { r.bytes() };
    let off: usize = meta.lines().find_map(|l| l.strip_prefix("func 0 ")).and_then(|l| l.split_whitespace().nth(1)).map(|h| usize::from_str_radix(h.trim_start_matches("0x"), 16).unwrap()).expect("func 0 offset");
    assert_eq!(off, c.entry(Func(0)) - r.address(), "the entry offset");
    assert_eq!(bin.len(), fresh.len());
    let words = |b: &[u8]| b.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).map(|w| if w >> 26 == 0b100101 { 0 } else { w }).collect::<Vec<u32>>();
    assert_eq!(words(&bin), words(fresh), "the .bin is the mapped code");
    drop(c);
    // Off: an empty directory writes nothing on the next miss.
    *DIR.lock().unwrap() = Some(String::new());
    let before = std::fs::read_dir(&out).unwrap().count();
    let (_, o2) = gravity_backend::mapped_cached_all(&program(true), &[Func(0)]).unwrap_or_else(|e| panic!("{}", e.message()));
    assert_ne!(o2.key.0, o.key.0, "the tweak is a second key");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), before, "an empty directory wrote files");
    if std::env::var("GRAVITY_DUMP_KNOB_KEEP").is_ok() {
        eprintln!("kept {}", out.display());
    } else {
        let _ = std::fs::remove_dir_all(&out);
    }
}

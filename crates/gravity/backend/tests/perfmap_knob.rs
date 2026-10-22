//! The JIT perf map (`gravity_backend::perfmap`, `pgrust.gravity_jit_perfmap`): a cache miss with the map on appends one
//! `START SIZE gravity:<key8>:<func>` line per function to `/tmp/perf-<pid>.map`, each the function's mapped range (its
//! entry, its symbol's size, inside the module's mapping, none overlapping) and a sidecar line naming the statement; a
//! note names the function's plan node; with the map off a miss writes nothing. The program is TH14-pgrc's fold.
#![cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]

use gravity_backend::perfmap;
use gravity_ir::text::parse_program;
use gravity_ir::Func;
use std::sync::atomic::{AtomicU8, Ordering};

static MODE: AtomicU8 = AtomicU8::new(perfmap::OFF);

fn mode() -> u8 {
    MODE.load(Ordering::Relaxed)
}

fn statement() -> String {
    "SELECT th14 fold (perfmap_knob test)".into()
}

fn program(tweak: u64) -> gravity_ir::Program {
    let text = include_str!("../tpde/tests/fixtures/th14_addhi_exhaustion.txt");
    let text = text.replacen("(const i64 4611686018427387904)", &format!("(const i64 {})", 4611686018427387904u64 + tweak), 1);
    parse_program(&text, &|_| Some(0x1000)).unwrap_or_else(|e| panic!("{e}"))
}

/// The map's lines for `key`: (start, size, name).
fn lines_of(key: u128) -> Vec<(usize, usize, String)> {
    let tag = format!("gravity:{:08x}:", (key >> 96) as u32);
    let text = std::fs::read_to_string(perfmap::paths().0).unwrap_or_default();
    text.lines()
        .filter(|l| l.contains(&tag))
        .map(|l| {
            let mut f = l.splitn(3, ' ');
            let start = usize::from_str_radix(f.next().unwrap(), 16).unwrap();
            let size = usize::from_str_radix(f.next().unwrap(), 16).unwrap();
            (start, size, f.next().unwrap().to_string())
        })
        .collect()
}

#[test]
fn a_miss_maps_each_function_and_off_maps_nothing() {
    perfmap::install(perfmap::Hooks { mode, statement });
    MODE.store(perfmap::FUNC, Ordering::Relaxed);
    let p = program(0);
    let (codes, o) = gravity_backend::mapped_cached_all(&p, &[Func(0)]).unwrap_or_else(|e| panic!("{}", e.message()));
    let ls = lines_of(o.key.0);
    assert_eq!(ls.len(), p.funcs.len(), "one line per function: {ls:?}");
    let ctx = mcx::MemoryContext::new("perfmap-knob");
    let fresh = gravity_tpde::compile(ctx.mcx(), &p).unwrap_or_else(|e| panic!("{}", e.message()));
    let side = std::fs::read_to_string(perfmap::paths().1).expect("the sidecar");
    let sl = side.lines().find(|l| l.starts_with(&format!("key {:032x} ", o.key.0))).unwrap_or_else(|| panic!("no sidecar line: {side}"));
    assert!(sl.ends_with("stmt SELECT th14 fold (perfmap_knob test)"), "{sl}");
    let base = sl.split_whitespace().skip_while(|w| *w != "base").nth(1).map(|h| usize::from_str_radix(h, 16).unwrap()).expect("base");
    let len = o.bytes;
    for (i, f) in p.funcs.iter().enumerate() {
        let name = format!("gravity:{:08x}:{}", (o.key.0 >> 96) as u32, f.name);
        let (start, size, _) = ls.iter().find(|l| l.2 == name).unwrap_or_else(|| panic!("no line {name}: {ls:?}"));
        assert_eq!(*size, fresh.size(Func(i as u32)), "{name}: the symbol's size");
        assert_eq!(start - base, fresh.entry(Func(i as u32)) - fresh.mapper().get_mapped_range().unwrap().address(), "{name}: the entry's offset");
        assert!(*start >= base && start + size <= base + len, "{name}: inside the mapping {base:#x}+{len:#x}");
    }
    assert_eq!(ls.iter().find(|l| l.2.ends_with(&format!(":{}", p.funcs[0].name))).unwrap().0, codes[0].entry() as usize, "f0's line is its entry");
    let mut rs: Vec<(usize, usize)> = ls.iter().map(|l| (l.0, l.1)).collect();
    rs.sort();
    assert!(rs.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "no two ranges overlap: {rs:?}");
    drop(fresh);
    // A note names each function's plan node; the note's mode maps a compile on a thread whose own mode is off.
    MODE.store(perfmap::OFF, Ordering::Relaxed);
    let q = program(1);
    let k = gravity_backend::cache::key_of(&q).0;
    perfmap::note(k, perfmap::FUNC, &[Func(0)], &[7], None);
    let (_, o2) = gravity_backend::mapped_cached_all(&q, &[Func(0)]).unwrap_or_else(|e| panic!("{}", e.message()));
    assert_eq!(o2.key.0, k);
    let ls = lines_of(k);
    assert!(ls.iter().any(|l| l.2 == format!("gravity:{:08x}:{}:n7", (k >> 96) as u32, q.funcs[0].name)), "{ls:?}");
    // Off with no note: a miss writes nothing.
    let r = program(2);
    let (_, o3) = gravity_backend::mapped_cached_all(&r, &[Func(0)]).unwrap_or_else(|e| panic!("{}", e.message()));
    assert!(lines_of(o3.key.0).is_empty(), "off wrote lines");
    // Tags: a tagged compile splits each function into its tags' ranges, none overlapping, together the function's whole
    // range from its entry; the code is the same as an untagged compile's.
    let t = program(3);
    let k = gravity_backend::cache::key_of(&t).0;
    let hand = |t: &gravity_ir::Program| {
        let b0 = t.funcs[0].blocks[0].insts.len() as u32;
        let mut tags = gravity_ir::JitTags::new(None);
        let (first, rest) = (tags.intern(format_args!("first")), tags.intern(format_args!("rest")));
        assert_eq!((first, rest, tags.intern(format_args!("fir{}", "st"))), (1, 2, 1));
        tags.push(gravity_ir::JitRun { func: 0, block: 0, idx: 0, tag: first });
        tags.push(gravity_ir::JitRun { func: 0, block: 0, idx: b0 / 2, tag: rest });
        for b in 1..t.funcs[0].blocks.len() as u32 {
            tags.push(gravity_ir::JitRun { func: 0, block: b, idx: 0, tag: rest });
        }
        tags
    };
    perfmap::note(k, perfmap::TAGS, &[], &[], Some(hand(&t)));
    let side = std::fs::read_to_string(perfmap::paths().1).expect("the sidecar");
    assert!(side.lines().any(|l| l.starts_with(&format!("key {k:032x} lowered mode 2 stmt "))), "the note's lowered line: {side}");
    let (codes, _) = gravity_backend::mapped_cached_all(&t, &[Func(0)]).unwrap_or_else(|e| panic!("{}", e.message()));
    let f0 = format!("gravity:{:08x}:{}:", (k >> 96) as u32, t.funcs[0].name);
    let mine: Vec<(usize, usize, String)> = lines_of(k).into_iter().filter(|l| l.2.starts_with(&f0)).collect();
    assert!(mine.iter().any(|l| l.2.ends_with(":first")) && mine.iter().any(|l| l.2.ends_with(":rest")), "{mine:?}");
    let plain = gravity_tpde::compile(ctx.mcx(), &t).unwrap_or_else(|e| panic!("{}", e.message()));
    let tags = hand(&t);
    let tagged = gravity_tpde::compile_tagged(ctx.mcx(), &t, |_| {}, Some(&tags)).unwrap_or_else(|e| panic!("{}", e.message()));
    let size = plain.size(Func(0));
    let mut rs: Vec<(usize, usize)> = mine.iter().map(|l| (l.0, l.1)).collect();
    rs.sort();
    assert_eq!(rs[0].0, codes[0].entry() as usize, "the first range starts at the entry: {mine:?}");
    assert!(rs.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0), "contiguous, none overlapping: {mine:?}");
    assert_eq!(rs.last().map(|r| r.0 + r.1), Some(rs[0].0 + size), "they end at the function's end: {mine:?}");
    assert!(plain.ranges(Func(0)).next().is_none() && tagged.ranges(Func(0)).next().is_some());
    for i in 0..t.funcs.len() as u32 {
        assert_eq!(plain.size(Func(i)), tagged.size(Func(i)), "f{i}: the same size tagged");
    }
    assert_eq!(plain.code_bytes(), tagged.code_bytes());
    #[cfg(target_arch = "aarch64")]
    {
        let bytes = |c: &gravity_tpde::Compiled<'_>| {
            let r = c.mapper().get_mapped_range().unwrap();
            let b = unsafe { r.bytes() };
            b.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).map(|w| if w >> 26 == 0b100101 { 0 } else { w }).collect::<Vec<u32>>()
        };
        assert_eq!(bytes(&plain), bytes(&tagged), "the tagged compile's code is the plain one's");
    }
}

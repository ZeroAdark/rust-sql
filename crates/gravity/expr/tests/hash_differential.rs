//! The IR half of the one key hash equals the runtime half bit for bit (lowerv4 M0): `gravity_expr::emit::hash` against
//! `gravity_abi::hash` on the interpreter, TPDE and wasm, over images of 0..=4 words (random and structured words: zero,
//! all-ones, the sign bit, top-bits-only, small ints), run-time and lowering-time bit counts, nullable images with their
//! flags word, and image equality in registers and against stored words.
use gravity_abi::hash::{route_and_full, Depth, KeyHash};
use gravity_expr::emit::hash::{emit_home, emit_home_at, emit_home_shift, emit_image, emit_image_eq, emit_image_eq_at, emit_key_hash, emit_part, emit_route_and_full, emit_split, emit_stored, emit_sub, Bits, ImageValues, KeyPart};
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::{ConvOp, Func, IntBinOp, IntCond, Program, Type};

/// The frame the test function reads and writes (all words): the image, a second stored image, the run-time bit counts,
/// the null mask, then the outputs.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct Frame {
    words: [u64; 4],
    other: [u64; 4],
    cap: u64,
    used: u64,
    pbits: u64,
    depth: u64,
    nulls: u64,
    out: [u64; 15],
}
const OUT: i32 = 104;
const CONST_CAP: u32 = 12;
const CONST_USED: u32 = 4;
const CONST_P: u32 = 4;
const CONST_D: u32 = 2;

/// `f(frame) -> 0` over an image of `n` words; the route is the first `n / 2` words.
fn program(n: usize) -> (Program, Func) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("keyhash", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let fr = b.params()[0];
    let words: Vec<_> = (0..n).map(|i| b.load(Type::I64, fr, 8 * i as i32)).collect();
    let [cap, used, pbits, depth, nulls] = [64, 72, 80, 88, 96].map(|o| b.load(Type::I64, fr, o));
    let image = ImageValues { words: words.clone() };
    let h = emit_key_hash(&mut b, &image);
    let (route, _) = emit_route_and_full(&mut b, &image, n / 2);
    let mut outs = vec![h, route];
    outs.push(emit_home(&mut b, h, Bits::Dyn(cap), Bits::Dyn(used)));
    outs.push(emit_part(&mut b, h, Bits::Dyn(pbits)));
    outs.push(emit_sub(&mut b, h, Bits::Dyn(pbits), Bits::Dyn(depth)));
    outs.push(emit_split(&mut b, h, Bits::Dyn(used)));
    outs.push(emit_stored(&mut b, h));
    outs.push(emit_home(&mut b, h, Bits::Const(CONST_CAP), Bits::of(Depth::new(CONST_USED, 0).unwrap())));
    outs.push(emit_part(&mut b, h, Bits::Const(CONST_P)));
    outs.push(emit_sub(&mut b, h, Bits::Const(CONST_P), Bits::Const(CONST_D)));
    outs.push(emit_split(&mut b, h, Bits::Const(CONST_USED + 3)));
    let own = emit_image_eq_at(&mut b, &image, fr, 0);
    let other = emit_image_eq_at(&mut b, &image, fr, 32);
    let eqs = emit_image_eq(&mut b, &image, &image);
    let both = b.int_bin(IntBinOp::And, own, eqs);
    outs.push(b.conv(ConvOp::Zext, Type::I64, both));
    outs.push(b.conv(ConvOp::Zext, Type::I64, other));
    // every word its own nullable key: null k when bit k of the mask is set
    let one = b.iconst(Type::I64, 1);
    let zero = b.iconst(Type::I64, 0);
    let flags: Vec<_> = (0..n)
        .map(|i| {
            let at = b.iconst(Type::I64, i as i64);
            let s = b.int_bin(IntBinOp::Ushr, nulls, at);
            let bit = b.int_bin(IntBinOp::And, s, one);
            b.icmp(IntCond::Ne, bit, zero)
        })
        .collect();
    let parts: Vec<KeyPart> = (0..n).map(|i| KeyPart { words: &words[i..i + 1], null: Some(flags[i]) }).collect();
    let nullable = emit_image(&mut b, &parts);
    outs.push(emit_key_hash(&mut b, &nullable));
    // D3326: the home over a shift taken once, for capacity bits of at least 1 (a zero `cap` reads as 1 here)
    let none = b.icmp(IntCond::Eq, cap, zero);
    let cap1 = b.select(none, one, cap);
    let shift = emit_home_shift(&mut b, cap1);
    outs.push(emit_home_at(&mut b, h, shift, Bits::Dyn(used)));
    for (i, v) in outs.into_iter().enumerate() {
        b.store(fr, OUT + 8 * i as i32, v);
    }
    b.ret(&[zero]);
    drop(b);
    (pb.finish(), f)
}

/// What the runtime half says for `fr` (the IR half must write exactly this).
fn expect(n: usize, fr: &Frame) -> [u64; 15] {
    let img = &fr.words[..n];
    let h = KeyHash::of(img);
    let (route, full) = route_and_full(img, n / 2);
    assert_eq!(full, h);
    let (cap, used, pbits, depth) = (fr.cap as u32, fr.used as u32, fr.pbits as u32, fr.depth as u32);
    let mut nullable: Vec<u64> = (0..n).map(|i| if fr.nulls >> i & 1 == 1 { 0 } else { img[i] }).collect();
    if n > 0 {
        nullable.push(fr.nulls & ((1 << n) - 1));
    }
    [
        h.to_abi(),
        route.to_abi(),
        h.home(cap, Depth::new(used, 0).unwrap()) as u64,
        h.part(pbits) as u64,
        h.sub(pbits, depth) as u64,
        h.split(Depth::new(used, 0).unwrap()) as u64,
        h.stored() as u64,
        h.home(CONST_CAP, Depth::new(CONST_USED, 0).unwrap()) as u64,
        h.part(CONST_P) as u64,
        h.sub(CONST_P, CONST_D) as u64,
        h.split(Depth::new(CONST_USED + 3, 0).unwrap()) as u64,
        1,
        (fr.other[..n] == fr.words[..n]) as u64,
        KeyHash::of(&nullable).to_abi(),
        h.home(cap.max(1), Depth::new(used, 0).unwrap()) as u64,
    ]
}

/// The wasm arm: one store with a linear memory, a function table and the shadow stack pointer (the wasm tests' rig).
struct Wasm {
    store: wasmtime::Store<()>,
    memory: wasmtime::Memory,
    func: wasmtime::Func,
}

const DATA: usize = 0x1_0000;

impl Wasm {
    fn new(p: &Program, f: Func) -> Wasm {
        use wasmtime::*;
        let bytes = gravity_wasm::compile(p, f).unwrap_or_else(|e| panic!("wasm compile: {e}"));
        let engine = Engine::new(Config::new().wasm_simd(true)).unwrap();
        let mut store = Store::new(&engine, ());
        let memory = Memory::new(&mut store, MemoryType::new(64, None)).unwrap();
        let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 16, None), Ref::Func(None)).unwrap();
        let sp = Global::new(&mut store, GlobalType::new(ValType::I32, Mutability::Var), Val::I32(DATA as i32)).unwrap();
        let module = Module::new(&engine, &bytes).unwrap();
        let inst = Instance::new(&mut store, &module, &[memory.into(), table.into(), sp.into()]).unwrap();
        let func = inst.get_func(&mut store, &gravity_wasm::func_export_name(f)).expect("exported");
        Wasm { store, memory, func }
    }

    fn run(&mut self, fr: &mut Frame) {
        let size = std::mem::size_of::<Frame>();
        let bytes = unsafe { std::slice::from_raw_parts(fr as *const Frame as *const u8, size) };
        self.memory.data_mut(&mut self.store)[DATA..DATA + size].copy_from_slice(bytes);
        let mut r = [wasmtime::Val::I64(-1)];
        self.func.call(&mut self.store, &[wasmtime::Val::I64(DATA as i64)], &mut r).unwrap();
        assert_eq!(r[0].i64(), Some(0));
        let back = self.memory.data(&self.store)[DATA..DATA + size].to_vec();
        unsafe { std::ptr::copy_nonoverlapping(back.as_ptr(), fr as *mut Frame as *mut u8, size) };
    }
}

#[test]
fn the_ir_half_is_the_runtime_half_on_every_backend() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let structured = [0u64, 1, u64::MAX, 1 << 63, 1 << 48, 7 << 56, 42, (-5i64) as u64, 0x8000_0000, 0xffff_ffff];
    for n in 0..=4usize {
        let (p, f) = program(n);
        gravity_ir::verify::verify(&p).unwrap_or_else(|e| panic!("verify n={n}: {e:?}"));
        let tpde = gravity_backend::mapped(&p, f).unwrap_or_else(|e| panic!("tpde: {}", e.message()));
        let interp = gravity_backend::interp(p.clone(), f).unwrap();
        let mut wasm = Wasm::new(&p, f);
        for case in 0..4000u32 {
            let mut fr = Frame::default();
            for w in fr.words.iter_mut() {
                *w = if case % 3 == 0 { structured[(next() % structured.len() as u64) as usize] } else { next() };
            }
            fr.other = fr.words;
            if case % 2 == 1 && n > 0 {
                fr.other[(next() % n as u64) as usize] ^= 1 << (next() % 64);
            }
            // D446: route bits up to ROUTE_BITS (58), a table's home bits up to 32 whatever is left below them (a home
            // past the hash's last bit reads zeros); one case in four a sub-split at depth 11 or more (a path past 32 bits)
            let rb = gravity_abi::hash::ROUTE_BITS as u64;
            fr.used = next() % (rb + 1);
            fr.cap = next() % 33;
            fr.pbits = if case % 4 == 0 { next() % 12 } else { next() % 33 };
            fr.depth = next() % ((rb - fr.pbits) / 3 + 1);
            if case % 4 == 0 {
                fr.depth = fr.depth.max(11);
            }
            fr.nulls = next();
            let want = expect(n, &fr);
            for (name, code) in [("tpde", &tpde), ("interp", &interp)] {
                let mut got = fr;
                // SAFETY: the frame is the ctx the function reads and writes; it is live for the call.
                assert_eq!(unsafe { code.call((&mut got as *mut Frame).cast()) }, 0);
                assert_eq!(got.out, want, "{name} n={n} {fr:?}");
            }
            let mut got = fr;
            wasm.run(&mut got);
            assert_eq!(got.out, want, "wasm n={n} {fr:?}");
        }
    }
}

/// CAP-28: an image of more than 64 nullable keys carries one flags word per 64 keys (key k's bit k % 64 of word k / 64),
/// so a NULL in key k and one in key k + 64 image apart (one wrapped word made them alike); the image's KeyHash is abi's,
/// on the interpreter and TPDE.
#[test]
fn a_nullable_image_past_64_keys_takes_one_flags_word_per_64_keys() {
    const N: usize = 130;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("wide", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let fr = b.params()[0];
    let (one, zero) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 0));
    let words: Vec<_> = (0..N).map(|i| b.load(Type::I64, fr, 8 * i as i32)).collect();
    let nulls: Vec<_> = (0..N)
        .map(|i| {
            let m = b.load(Type::I64, fr, 8 * (N + i / 64) as i32);
            let at = b.iconst(Type::I64, (i % 64) as i64);
            let s = b.int_bin(IntBinOp::Ushr, m, at);
            let bit = b.int_bin(IntBinOp::And, s, one);
            b.icmp(IntCond::Ne, bit, zero)
        })
        .collect();
    let parts: Vec<KeyPart> = (0..N).map(|i| KeyPart { words: &words[i..i + 1], null: Some(nulls[i]) }).collect();
    let img = emit_image(&mut b, &parts);
    assert_eq!(img.words.len(), N + 3, "130 image words, then three flags words");
    let out = 8 * (N + 3) as i32;
    for (i, &w) in img.words.iter().enumerate() {
        b.store(fr, out + 8 * i as i32, w);
    }
    let h = emit_key_hash(&mut b, &img);
    b.store(fr, out + 8 * (N + 3) as i32, h);
    b.ret(&[zero]);
    drop(b);
    let p = pb.finish();
    gravity_ir::verify::verify(&p).unwrap_or_else(|e| panic!("verify: {e:?}"));
    let tpde = gravity_backend::mapped(&p, f).unwrap_or_else(|e| panic!("tpde: {}", e.message()));
    let interp = gravity_backend::interp(p.clone(), f).unwrap();
    let mut images = Vec::new();
    for nulls in [[1u64, 0, 0], [0, 1, 0], [0, 0, 1], [1, 1, 3], [u64::MAX, 0, 2]] {
        let mut want: Vec<u64> = (0..N).map(|i| if nulls[i / 64] >> (i % 64) & 1 == 1 { 0 } else { 7 + i as u64 }).collect();
        want.extend(nulls);
        want.push(KeyHash::of(&want).to_abi());
        for (name, code) in [("tpde", &tpde), ("interp", &interp)] {
            let mut fr: Vec<u64> = (0..N as u64).map(|i| 7 + i).chain(nulls).chain([0; N + 4]).collect();
            // SAFETY: the frame holds every word the function reads and writes; it is live for the call.
            assert_eq!(unsafe { code.call(fr.as_mut_ptr().cast()) }, 0);
            assert_eq!(fr[N + 3..], want[..], "{name} nulls {nulls:?}");
        }
        images.push(want);
    }
    assert_ne!(images[0], images[1], "a NULL in key 0 vs a NULL in key 64");
    assert_ne!(images[1], images[2], "a NULL in key 64 vs a NULL in key 128");
}

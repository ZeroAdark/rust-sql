//! Zero-index `ptr_add` (ISA audit 2026-09-25): the lowerer reads its per-table state as `ptr_add (stack_addr s) 0 1 0`,
//! then `ptr_add _ 0 1 112` and a load. The first is a copy of its base (`FnView::propagate_copies`), and a constant index
//! folds into the load's immediate whether or not the constant is materialized for other uses (`fuse_addresses`). So the
//! read is one `ldr x, [base, #112]`, where the old code was `mov; mov #0; add; ldr`. Runs natively and on the
//! interpreter; on AArch64 the functions keep no `add` of a zero index, no register copy of the base, and no
//! materialized zero.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;

fn load(src: &str) -> Program {
    parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"))
}
fn run(p: &Program, name: &str, args: &[u64]) -> u64 {
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    common::both(p, f, args).unwrap()[0]
}

#[cfg(target_arch = "aarch64")]
fn disasm(p: &Program, name: &str) -> Vec<String> {
    use mcx::MemoryContext;
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    let ctx = MemoryContext::new("ptr_zero");
    let c = gravity_tpde::compile(ctx.mcx(), p).unwrap();
    let range = c.mapper().get_mapped_range().unwrap();
    let bytes = unsafe { range.bytes() };
    let off = c.entry(f) - range.address();
    let mut out = Vec::new();
    let mut i = off;
    while i + 4 <= bytes.len() {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let mut s = [0u8; 128];
        let ddi = tpde_disarm::decode(w);
        out.push(tpde_disarm::da64_format(&ddi, &mut s).map(|n| String::from_utf8_lossy(&s[..n]).into_owned()).unwrap_or_else(|_| "??".into()));
        if w == 0xd65f03c0 {
            break;
        }
        i += 4;
    }
    out
}

/// `state`: a one-key count's shape over a pointer argument. `z` is also returned, so it is materialized (the old code then
/// added it: `mov #0; add`), and `b` has two loads at different offsets, so no load fusion takes it.
/// `slot`: that state slot itself, `ptr_add (stack_addr 0) 0` with two stores and two loads through it. `far`: a
/// constant index whose displacement no load immediate takes (the `add` stays; still correct).
const PTR: &str = r#"
(program
  (fn state ((p ptr)) (i64 i64)
    (block entry ((p ptr))
      (z (const i64 0))
      (s (ptr_add p z 1 0))
      (a (ptr_add s z 1 112))
      (x (load i64 a 0))
      (b (ptr_add p z 1 0))
      (y (load i64 b 0))
      (w (load i64 b 16))
      (xy (iadd x y))
      (r (iadd xy w))
      (return r z)))
  (fn slot ((v i64)) (i64 i64)
    (slots (128 8))
    (block entry ((v i64))
      (z (const i64 0))
      (st (stack_addr 0))
      (s (ptr_add st z 1 0))
      ((store s 112 v))
      ((store s 104 v))
      (x (load i64 s 112))
      (u (load i64 s 104))
      (xu (iadd x u))
      (return xu z)))
  (fn far ((p ptr)) (i64)
    (block entry ((p ptr))
      (k (const i64 100000))
      (a (ptr_add p k 1 0))
      (x (load i64 a 0))
      (return x))))
"#;

#[test]
fn a_zero_index_state_read_is_one_load() {
    let p = load(PTR);
    let mut buf = [0i64; 16];
    for (i, x) in buf.iter_mut().enumerate() {
        *x = (i as i64 + 1) * 1000 + 7;
    }
    let want = buf[14] + buf[0] + buf[2];
    assert_eq!(run(&p, "state", &[buf.as_mut_ptr() as u64]) as i64, want);
    for v in [0i64, 1, -1, i64::MIN, i64::MAX] {
        assert_eq!(run(&p, "slot", &[v as u64]) as i64, v.wrapping_mul(2), "slot {v}");
    }
    let big = vec![0x5a5a_i64; 100_000 / 8 + 2];
    let off_elems = 100_000 / 8;
    let p0 = unsafe { big.as_ptr().add(off_elems).cast::<u8>().sub(100_000) } as u64;
    assert_eq!(run(&p, "far", &[p0]) as i64, 0x5a5a);
    #[cfg(target_arch = "aarch64")]
    {
        let lines = disasm(&p, "state");
        let text = lines.join("\n");
        // the three loads, two `iadd`s and nothing that adds a zero index or copies the base
        assert_eq!(lines.iter().filter(|l| l.starts_with("ldr x")).count(), 3, "three loads:\n{text}");
        let adds = lines.iter().filter(|l| l.starts_with("add x") && !l.contains("sp")).count();
        assert_eq!(adds, 2, "only the two iadds (no zero-index add):\n{text}");
        assert!(lines.iter().any(|l| l.starts_with("ldr x") && (l.contains("#112") || l.contains("#0x70"))), "no ldr [p, #112]:\n{text}");
        let lines = disasm(&p, "slot");
        let text = lines.join("\n");
        // the loads forward from the stores; the stores take the slot's address with an immediate
        assert!(lines.iter().any(|l| l.starts_with("str x") && (l.contains("#112") || l.contains("#0x70"))), "slot: no str [slot, #112]:\n{text}");
        // adds off the frame: only the `iadd` (the old code also added the materialized zero to the slot address)
        let rr = lines.iter().filter(|l| l.starts_with("add x") && !l.contains("sp") && !l.contains("x29")).count();
        assert_eq!(rr, 1, "slot: a zero-index add:\n{text}");
    }
}

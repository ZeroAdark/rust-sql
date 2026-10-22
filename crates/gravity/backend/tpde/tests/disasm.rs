//! Prints the disassembly of a small program (debug aid). AArch64 only: the
//! decoder is disarm.
#![cfg(target_arch = "aarch64")]
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use mcx::MemoryContext;

#[test]
fn dump_add() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::I64, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let [x, y]: [Value; 2] = b.params().try_into().unwrap();
    let r = b.int_bin(IntBinOp::Add, x, y);
    b.ret(&[r]);
    let p = pb.finish();
    let ctx = MemoryContext::new("disasm");
    let c = gravity_tpde::compile(ctx.mcx(), &p).unwrap();
    let range = c.mapper().get_mapped_range().unwrap();
    let bytes = unsafe { range.bytes() };
    let entry = c.entry(f);
    let off = entry - range.address();
    eprintln!("entry at +{off:#x}, {} bytes mapped", bytes.len());
    let mut i = off;
    while i + 4 <= bytes.len() && i < off + 256 {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let mut s = [0u8; 128];
        let ddi = tpde_disarm::decode(w);
        let text = tpde_disarm::da64_format(&ddi, &mut s).map(|n| String::from_utf8_lossy(&s[..n]).into_owned()).unwrap_or_else(|_| "??".into());
        eprintln!("{:#06x}: {w:08x}  {text}", i - off);
        if w == 0xd65f03c0 { break; }
        i += 4;
    }
}

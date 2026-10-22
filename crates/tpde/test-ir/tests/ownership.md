The parser copies names into one budgeted buffer; no borrow of input survives parsing.
Records and the owning TestIR contain no destructors. Retained resolver scratch
uses source offsets, so input text may be destroyed after every parse result.
`clear` retains model and parser capacities; `release` retires their buffers.
Individually deallocating contexts return allocations immediately; bump contexts
retain physical storage and charge until reset. Context reset reclaims all arena
storage. `retained_bytes` counts array bytes and usable hash-entry payload capacity;
it excludes hash control bytes and allocator overhead. Context usage is the full
accounting authority.
An adaptor borrows its module, preventing clear, release, or parse during analysis.

```compile_fail
let context = mcx::MemoryContext::new_bump("test-ir");
let mut ir = tpde_test_ir::TestIR::new(context.mcx());
ir.parse_ir("f() {\nb:\nterminate\n}\n").unwrap();
let adaptor = tpde_test_ir::TestIRAdaptor::new(&ir);
ir.clear();
use tpde_core::ir_adaptor::IrAdaptor;
assert_eq!(adaptor.func_count(), 1);
```

```compile_fail
let mut context = mcx::MemoryContext::new_bump("test-ir");
let ir = tpde_test_ir::TestIR::new(context.mcx());
context.reset();
assert_eq!(ir.functions.len(), 0);
```

Borrowed PHI slots cannot be used after the IR storage is cleared.

```compile_fail
use tpde_core::ir_adaptor::{IrAdaptor, PhiRef};
let context = mcx::MemoryContext::new_bump("test-ir");
let mut ir = tpde_test_ir::TestIR::new(context.mcx());
ir.parse_ir("f(%a) {\nb:\n%x = phi [^b, %a]\nret %x\n}\n").unwrap();
let adaptor = tpde_test_ir::TestIRAdaptor::new(&ir);
let phi = adaptor.val_as_phi(tpde_test_ir::IRValueRef(1));
ir.clear();
assert_eq!(phi.incoming_count(), 1);
```

A parsed module cannot outlive its memory context.

```compile_fail
fn escape<'m>() -> tpde_test_ir::TestIR<'m> {
    let context = mcx::MemoryContext::new_bump("temporary");
    tpde_test_ir::TestIR::new(context.mcx())
}
```

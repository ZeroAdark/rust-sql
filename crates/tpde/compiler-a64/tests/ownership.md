The retained target scratch cannot outlive its memory context.

```compile_fail
use mcx::MemoryContext;
use tpde_compiler_a64::CompilerA64;
let target;
{
    let context = MemoryContext::new("short compiler lifetime");
    target = CompilerA64::new(context.mcx(), (), 0);
}
let _ = target.state();
```

Borrowed frontend configuration remains borrowed for the target's lifetime.

```compile_fail
use mcx::MemoryContext;
use tpde_compiler_a64::CompilerA64;
let context = MemoryContext::new("compiler lifetime");
let target;
{
    let configuration = [13u8; 4];
    target = CompilerA64::new(context.mcx(), &configuration, 0);
}
let _ = target.frontend[0];
```

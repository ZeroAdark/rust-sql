A target cannot outlive its accounting context.

```compile_fail,E0597
use mcx::MemoryContext;
use tpde_compiler_x64::CompilerX64;
let target;
{
    let context = MemoryContext::new_bump("x64");
    target = CompilerX64::new(context.mcx(), (), 0);
}
drop(target);
```

Borrowed return offsets prevent mutation.

```compile_fail,E0502
use mcx::MemoryContext;
use tpde_compiler_x64::{CompilerX64, X64CompilerState};
let context = MemoryContext::new_bump("x64");
let mut target = CompilerX64::new(context.mcx(), (), 0);
target.push_return(4).unwrap();
let offsets = target.return_offsets();
target.clear_returns();
std::hint::black_box(offsets);
```

Moving a target transfers its retained storage.

```compile_fail,E0382
use mcx::MemoryContext;
use tpde_compiler_x64::{CompilerX64, X64CompilerState};
let context = MemoryContext::new_bump("x64");
let target = CompilerX64::new(context.mcx(), (), 0);
let moved = target;
std::hint::black_box(target.x64());
drop(moved);
```


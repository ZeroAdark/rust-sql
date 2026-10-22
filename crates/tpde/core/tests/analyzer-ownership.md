An analysis borrow cannot survive a function switch.

```compile_fail
use mcx::ForgetSafe;
use tpde_core::{Analyzer, ir_adaptor::IrAdaptor};
use tpde_base::ValLocalIdx;
fn overwrite<A: IrAdaptor>(analyzer: &mut Analyzer<'_, A>, adaptor: &A, func: A::FuncRef)
where A::BlockRef: Copy + ForgetSafe, A::ValueRef: Copy, A::InstRef: Copy {
    let live = analyzer.liveness_info(ValLocalIdx::new(0));
    analyzer.switch_func(adaptor, func).unwrap();
    std::hint::black_box(live);
}
```

A layout slice also protects its function state.

```compile_fail
use mcx::ForgetSafe;
use tpde_core::{Analyzer, ir_adaptor::IrAdaptor};
fn overwrite<A: IrAdaptor>(analyzer: &mut Analyzer<'_, A>, adaptor: &A, func: A::FuncRef)
where A::BlockRef: Copy + ForgetSafe, A::ValueRef: Copy, A::InstRef: Copy {
    let blocks = &analyzer.block_layout[..];
    analyzer.switch_func(adaptor, func).unwrap();
    std::hint::black_box(blocks);
}
```

Analysis storage cannot escape its memory context.

```compile_fail
use mcx::{ForgetSafe, MemoryContext};
use tpde_core::{Analyzer, ir_adaptor::IrAdaptor};
fn escape<'m, A: IrAdaptor>() -> Analyzer<'m, A>
where A::BlockRef: Copy + ForgetSafe, A::ValueRef: Copy, A::InstRef: Copy {
    let context = MemoryContext::new_bump("temporary analyzer");
    Analyzer::new(context.mcx())
}
```

Ending a view permits the next analysis without retaining an adaptor borrow.

```no_run
use mcx::ForgetSafe;
use tpde_core::{Analyzer, ir_adaptor::IrAdaptor};
fn switch<A: IrAdaptor>(analyzer: &mut Analyzer<'_, A>, adaptor: &A, func: A::FuncRef)
where A::BlockRef: Copy + ForgetSafe, A::ValueRef: Copy, A::InstRef: Copy {
    let _previous_blocks = analyzer.block_layout.len();
    analyzer.switch_func(adaptor, func).unwrap();
}
```

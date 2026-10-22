The const parameter counts inline **64-bit words**: `SmallBitSet::<4>` maps to
upstream `SmallBitSet<256>`. Inline capacities not divisible by 64 cannot be
expressed. `move_from` preserves the C++ moved-from logical length, and
`move_assign` preserves its destination-clear/source-unchanged behavior.
Ordinary Rust moves consume the source. Rust self-assignment (`bits = bits`)
represents C++ self-move assignment, which leaves the value unchanged.

Copies are not admitted (the pinned C++ copy forms cannot instantiate):

```compile_fail
use tpde_util::SmallBitSet;
let context = mcx::MemoryContext::new_bump("copy");
let bits = SmallBitSet::<1>::new(context.mcx());
let _copy = bits.clone();
```

The backing context cannot be dropped while a bitset is live:

```compile_fail
use tpde_util::SmallBitSet;
let bits;
{
    let context = mcx::MemoryContext::new_bump("lifetime");
    bits = SmallBitSet::<1>::new(context.mcx());
}
assert_eq!(bits.bit_size(), 0);
```

Native moves prevent later access to the consumed value:

```compile_fail
use tpde_util::SmallBitSet;
let context = mcx::MemoryContext::new_bump("move");
let bits = SmallBitSet::<1>::new(context.mcx());
let moved = bits;
assert_eq!(bits.bit_size(), moved.bit_size());
```

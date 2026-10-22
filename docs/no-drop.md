# Zero drop overhead: the data/resource split

Fabled's numbers: drop glue was ~11% of planner instructions,
`drop_in_place<PlanStateNode>` ≈ 2 MiB of code, and retrofitting forget-arenas
was all-or-nothing (one `Vec` field anywhere gives a node graph drop glue
forever). The per-query spine is frontend-bound, so dead cleanup code costs
even when it never runs. Here the discipline is compile-enforced from M0.

## Data: no drop glue, ever, enforced by the compiler

Every arena API asserts at monomorphization time:

```rust
fn alloc<T>(&self, v: T) -> &'mcx T {
    const { assert!(!core::mem::needs_drop::<T>()) };
    // ...
}
```

`needs_drop` recurses the whole type, so this is not a hand-maintained trait —
a `String` three fields deep breaks the build at the allocation site. All
query-lifetime types (nodes, plans, executor state, tuple data) therefore use:

- `AVec<'mcx, T>` / arena slices, never `Vec`
- `&'mcx str` / arena strings, never `String`
- `&'mcx T` or `u32` handles, never `Box`
- arena-backed hashbrown + FxHash, never `HashMap`
- `Rc` only outside arenas, only where C refcounts

Never stage in a std collection and convert — build in the arena directly.
Reset = pointer rewind + block recycle to freelists (C's `context_freelists`);
zero per-object work at any object count. Lifetime classes mirror C's context
tree: per-message, per-statement, per-executor, per-tuple, per-transaction,
session.

Consequences we're buying beyond cheap reset:
- overwrite/assignment of `!needs_drop` types is pure memcpy — no
  drop-old-value branch on every store;
- `?` propagation and unwinding generate no cleanup landing pads — smaller
  cold-code footprint on the frontend-bound spine;
- `drop_in_place` monomorphizations vanish from the binary.

## Resources: few, droppy, concentrated in owners

Buffer pins, locks, fds, snapshot refs MUST release — they are thin RAII
guards living in per-statement/per-scan owner structures (fabled's proven
model: `commit()` consumes for ordered teardown + leak warnings; `Drop` is the
abort path; never a registry with release authority). Arena data cannot contain
a guard — the assert forbids it — so nodes hold handles into the owner. The
compile error at the border is the design: it recreates C's
palloc-vs-ResourceOwner split mechanically.

## Escape hatches (loud, greppable)

- `ManuallyDrop<T>` passes the assert by design — the explicit "leak like C"
  wrapper; each use justified in the PR.
- Cold-path droppy values (error payloads, DDL scratch) live on the plain heap
  (mimalloc); drop is fine where it's cold.

## Forget-owners (`ForgetSafe`)

Bulk owners that C frees by context reset (the planner's `PlannerRun`) are
wrapped in `mcx::ArenaForget<T: ForgetSafe>` — drop glue never runs, the arena
reclaims. `ForgetSafe` is censused per type by
`forget_safe_struct!/enum!/tuple!` (exhaustive destructure: a new field breaks
the build) with `forget_safe_nodrop!` const-proving no-drop leaves; exempt
fields sit after `;` with a justifying comment. `forget_box_in` leaks a single
shared object (`&'mcx IndexOptInfo`) the same way.

## Enforcement stack

1. `const { assert!(!needs_drop) }` in every arena alloc/collection API.
2. `impl Drop` allowed only in the resource-guard modules (audit checks).
3. `scripts/drop-glue-audit.sh <binary>` — nm the fat-LTO binary, diff
   `drop_in_place<…>` symbols against the allowlist; CI tripwire (inlined glue
   escapes nm, but the expensive walks are exactly the ones big enough to show).
4. /bench-crate tracks mallocs-per-query and reset cost; a drop regression
   surfaces as either.

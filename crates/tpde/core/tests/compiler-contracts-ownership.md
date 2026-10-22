Calling convention assigners borrow their `CCInfo`; policies are owned and
statically dispatched. The borrowed table may be shared by multiple assigners
and cannot be moved, replaced, or destroyed while any assigner still uses it.
Its fields are immutable even when no assigner borrows it. Construct a new
complete table to change its values; retargeting the assigner's public reference
requires another table with the same lifetime. The policy has no destructor,
and all four abstract operations are mandatory. `is_vararg` alone defaults to
false. No target ABI decisions are supplied by this carrier.

Compiler contracts describe implementations, without supplying compilation,
allocation, register-selection, or variable-reference algorithms. Every
compiler constructor must invoke `check_compiler`, matching the upstream
constructor's concept assertion. Configuration bank dimensions and variable
reference flags are checked there. The sealed special-value carrier has a
mutable byte at offset zero and a copyable payload; custom layouts cannot
self-certify this invariant.

`cur_cc_assigner` permits exclusive direct access to the selected assigner.
`with_cur_cc_assigner` lends that same selected state alongside the remaining
compiler owner, so prologue and return-building callbacks can mutate both.
The implementation must preserve the selected policy rather than clone it,
must restore ownership before returning or unwinding, and must reject nested
acquisition while that state is lent. The operation cannot return references
to either temporary borrow. This additional Rust contract replaces C++'s
unchecked same-owner pointer aliasing; it supplies no compiler algorithm.

The test owner demonstrates safe move-out from a private `Option`, runs the
callback, restores the option on success or caught panic, and resumes unwinding
after restoration. Its generic witness obtains the current assigner, resets it,
and invokes the same owner's `prologue_assign_arg`; a second witness checks
restoration and subsequent reset/reuse. Future compiler owners must implement
this boundary for their real state before caller wiring can qualify. The
fixture's catch/unwind code is test-only and is not a production allocation or
error-handling implementation.

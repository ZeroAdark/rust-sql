# Compiler contract reference protocol

Source: pristine TPDE revision `116b2b83eb8491fd63c017285c91b007b8a26a67`.
The oracle includes the complete upstream `CompilerBase.hpp`, `Compiler.hpp`,
`CompilerConfig.hpp`, and their transitive project headers, without replacement
headers or extracted declarations. `manifest.json` identifies every imported
file by Git blob and SHA-256, including both license notices. Build with modern Clang, C++20,
and a standard library supporting `<format>`; no TPDE object file is required.
The oracle supplies standard-library includes omitted by upstream headers and
undefines the leaked `ARG` macro after inclusion. TPDE_LOGGING stays undefined.
All execution is runner-only and initially **UNRUN**.

Select `fields`, `assigner`, `layout`, or `concepts` by ASCII stdin followed by newline
(or argv[1]). Output has no padding, pointers, platform-sized fields, or text.
All integers below use little-endian encoding. A CCAssignment record is nine
u32 words: `reg.id`, `consecutive`, `sret`, `byval`, `int_ext`, `align`,
`bank.id`, `size`, `stack_off`. Booleans are normalized to zero or one.

`fields` emits the default CCAssignment followed by 8192 field records. Nest
loops in this order: `v=0..255`, `flags=0..3`, `i=0..7`. The fields are
`v`, `v`, `flags&1`, `(flags>>1)&1`, `v^0x80`, `255-v`, `v`, `S[i]`,
`S[7-i]`, where `S=[0,1,7,8,255,65536,0x80000000,0xffffffff]`.
This exercises each complete byte domain, both bitfield booleans independently,
and zero, ordinary, high-bit, and maximum u32 sizes/offsets. The default record
contains `[255,0,0,0,0,0,255,0,0]`. Total output: 8193 records, 294948 bytes.
These are field-storage tests; no target ABI is inferred from unconstrained
byte values in this aggregate.

`assigner` begins with three 40-byte CCInfo/borrow records. Each has three u64
fields (`allocatable_regs`, `callee_saved_regs`, `arg_regs`), then four u32
fields (`red_zone_size`, `ccinfo == expected_address`,
`&get_ccinfo() == expected_address`, `is_vararg()`). The original CCInfo is
`[0xfedcba9876543210,0x8000000000000001,0x000000030000000c,0]`, with
`red_zone_size` omitted in C++ initialization to exercise its default.
The alternative is `[0xffffffffffffffff,0,0xaaaaaaaa55555555,128]`.
Records are: ordinary assigner borrowing original; the same assigner after its
public `ccinfo` member is redirected to alternative; a variadic subclass
borrowing original. Identity checks are one; `is_vararg` is zero, zero, one.
The first two dispatch through the actual upstream default virtual method;
the third dispatches to the test subclass override.

Next are three epochs, `epoch=0..2`. An epoch begins with `reset()` through an
upstream CCAssigner reference, then one u32 epoch followed by six u32 state
words: `stack_size`, `next_reg[0]`, `next_reg[1]`, `arg_calls`, `ret_calls`,
`reset_calls`. Reset clears the first five words and increments reset_calls.
Each epoch then has twelve `assign_arg` output rows and one `assign_ret` row.
Each row contains the nine-word assignment record followed by the six-word
state. Stack size is obtained through upstream `get_stack_size()` dispatch.

Argument `j=0..11` starts from a default CCAssignment, setting:
`consecutive=j%3`, `sret=(j==0)`, `byval=(j%5==0)`,
`int_ext=((j&1)?0x80:0)|(8<<(j%4))`, `align=1<<(j%5)`, `bank=j%2`,
`size=1<<(j%7)`. The test-only stateful assigner increments arg_calls, then
uses a register when `!byval && next_reg[bank]+consecutive+1 <= 4`:
`reg=bank*32+next_reg[bank]`, `next_reg[bank]+=consecutive+1`. Otherwise it
rounds stack_size up to `align`, writes invalid reg and stack_off=stack_size,
and adds size. It changes no other assignment fields.

The return assignment starts from default, setting `consecutive=2`,
`sret=epoch&1`, `int_ext=0x88`, `align=16`, `bank=epoch%2`, `size=16`,
`stack_off=0xffffffff`. assign_ret increments ret_calls and sets
`reg=bank*32+(sret?3:0)`, `stack_off=0`, preserving other fields and stack size.
Each epoch is 808 bytes; the whole mode is 2544 bytes.
This assigner is an interface fixture, not an implementation or oracle for any
upstream target calling convention. The actual upstream constructor, pointer,
borrow accessor, virtual method signatures, and default vararg method are the
behavior under test. No allocation, cancellation, or fallible operation exists
in this selected prerequisite.

`layout` emits fifteen u32 words: CCAssignment size and alignment; offsets of
reg, consecutive, int_ext, align, bank, size and stack_off; CCInfo size and
alignment; then the size, alignment, mode offset and payload offset of the
ordinary test `Special { u8 mode; u64 payload; }`. Rust compares these against
its concrete carriers on the same runner target. Bitfield storage is not read
as bytes, and arbitrary generic payloads do not claim a C++ ABI. Total: 60 bytes.

`concepts` emits three u32 words: protocol version 1, positive assertion count
43, and negative assertion count 112 (12 bytes), also recorded as three
whitespace-separated decimal words in `COUNTS`. Its successful compilation
proves the assertions against the actual source concepts. The assertion roster
and exact rejected clauses are named in `oracle.cpp`. All checks compile in
both assertion-enabled and NDEBUG builds; there are no intentionally malformed
translation units whose diagnostics could hide an unrelated compile failure.
The roster covers SameBaseAs cv/reference stripping (volatile remains), exact
configuration types, public unambiguous CCAssigner inheritance, all associated
types, exact C-array element type and bank dimensions, all Compiler callbacks,
convertible versus exact returns, all ValueParts clauses, mode member type,
standard layout and offset, optional special returns, required cur_cc_assigner,
and both branches of default/custom variable handling. Positive adversarial
cases record source requirements that are weaker than prose: mode values below
four, arbitrary byte size, and unchecked return types on statement-only hooks.
Zero banks remain a valid CompilerConfig. Checked Compiler admission additionally
requires a positive array bound; its upstream fixed-assignment clause uses a
standard C array. The zero-bank negative against GoodCompiler also has a fixed
array-width mismatch and therefore does not independently isolate the bound rule.

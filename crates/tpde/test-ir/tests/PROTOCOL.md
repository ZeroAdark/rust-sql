# TestIR reference protocol

`oracle.cpp` compiles the unchanged pinned upstream parser, printer, adaptor and
Analyzer. It reads a mode line (`parse`, `print`, `records`, `adaptor`, or
`analyze`), followed by the exact bytes of one TestIR module through EOF. No
expected parse or analysis results are input to the C++ executable. Each process
constructs one empty TestIR. All numbers below are unsigned decimal; booleans
are `0` or `1`; list fields are a count followed by that many IDs. Empty names
are represented by `-`, which cannot occur in an upstream identifier. Output
records end in LF with no trailing spaces.

Every admitted module invocation first emits `parse 0\n` or `parse 1\n`. Parse failure
emits nothing else and exits successfully in every mode. Parser error wording
is not part of the protocol; logging is disabled. A successful `parse` invocation
also stops after that line. Nonzero exit means transport failure, assertion
failure, or process failure, rather than a normal parser rejection.

The driver passes text unchanged. It rejects non-ASCII bytes and a semicolon on
the final physical line without a terminating LF, with exit 64 and a stderr
diagnostic. Upstream passes signed `char` to the cctype functions and can loop
forever or process the final comment character as code. Rust's defined behavior
for these cases is tested separately and is not claimed as upstream agreement.
All ordinary malformed comparison cases end in LF. Other than the activation
control below, a mode not listed above is a transport error with exit 64.

## `sanitizer` activation control

This oracle-only mode consumes no module and emits `sanitizer 0\n` or
`sanitizer 1\n` from the unchanged pinned header's
`tpde::util::address_sanitizer_active` constant. Its value reflects the C++
compiler's sanitizer feature/macros. Before the semantic corpus, the harness
requires Rust package activation to equal `tpde_util` activation and executes
this control in both C++ assertion modes. Ordinary and ASAN runner stages
respectively require inactive and active oracle markers.

## `print`

After `parse 1\n`, the driver calls upstream `TestIR::print()` directly. The
result starts with `Printing IR\n`. No formatting is replaced or normalized.

## `records`

The driver serializes the parser's four arrays in their stored order:

```text
module <function-count> <block-count> <value-count> <operand-count>
function <id> <name> <declaration> <local-only> <has-call> <arg-begin> <arg-end> <block-begin> <block-end>
block <id> <name> <succ-begin> <succ-end> <inst-begin> <phi-end> <inst-end> <info> <info2>
value <id> <name> <type> <op> <fixed> <call-target> <op-count> <op-begin> <op-end>
operands <count> <operand>...
```

There are exactly the declared counts of function, block, and value records.
Type and op are the numeric upstream enum discriminants. `call-target` is
4294967295 unless the op is `call`. Argument `op-count`, `op-begin`, and `op-end`
are serialized as zero. Those four fields are indeterminate in upstream when
inapplicable; the driver never reads their indeterminate storage. This mode does
not call the adaptor or Analyzer and does not inspect partially parsed arrays.

## `adaptor`

This mode calls the upstream adaptor's accessors and lifecycle methods. Function,
block, instruction, value, operand, and result order is the returned order.

```text
adaptor <invalid-value> <invalid-block> <invalid-function> <provides-highest> <visits-args> <function-count>
funcs <count> <function>...
compile <count> <function>...
function <id> <link-name> <extern> <local-only> <weak-linkage>
```

Each declaration or function without blocks ends its function section with
`skip`. Those parser-valid functions are outside `switch_func`'s nonempty-body
precondition. Every other function continues with:

```text
current <switch-success> <highest-local-value> <unwind> <vararg> <dynamic-alloca> <entry-block>
args <count> <value>...
arg <value> <byval> <byval-align> <byval-size> <sret>
allocas <count> <value>...
value <id> <formatted-name> <local-index> <is-phi> <ignore-liveness> <alloca-size> <alloca-align>
blocks <count> <block>...
```

There is one `arg` record per current argument, indexed in argument order when
calling ABI accessors. The `value` records cover every stored value from this
function's first argument through its last instruction. Alloca size and alignment
are 4294967295 for other ops. `allocas` contains only entry-block allocas as
returned by `cur_static_allocas`; value records also exercise accessors on
non-entry allocas.

Each current block then produces:

```text
block <id> <formatted-name> <old-info> <old-info2> <written-info> <written-info2>
succs <count> <block>...
phis <count> <value>...
insts <count> <instruction>...
```

The driver writes `id ^ 0xa5a5a5a5` and `id ^ 0x5a5a5a5a` with the two metadata
setters, reads them back, and restores the original values. For each PHI, before
the block's instructions, it writes:

```text
phi <value> <incoming-count>
incoming <slot> <incoming-value> <incoming-block> <lookup-value-for-that-block>
missing <lookup-value-for-invalid-block>
```

Incoming slots are traversed in ascending order. Duplicate predecessor entries
retain all slots, and lookup returns the first matching slot. A missing block
returns the adaptor's invalid-value sentinel. Each non-PHI instruction follows:

```text
inst <id> <formatted-name> <fused>
operands <count> <value>...
results <count> <value>...
```

After all blocks, the driver calls `reset` and emits
`reset <highest-local-value>`. `start_compile` and `end_compile` bracket the
module. They have no upstream output or state effect.

## `analyze`

One actual upstream `Analyzer<TestIRAdaptor>` is retained across all functions
in the module. The adaptor's `switch_func` runs before each Analyzer call.
Declaration and empty-body functions produce only `function <id> <name>` and
`skip`. Each other function produces:

```text
function <id> <name>
state <liveness-epoch> <max-local-value> <instruction-count>
layout <count> <block>...
map <count> <loop>...
loops <count>
loop <id> <level> <parent> <begin> <end> <block-count> <definitions> <child-definitions>
values <count>
live <local-index> <epoch> <first> <last> <reference-count> <lowest-common-loop> <last-full>
ignored <local-index> <stale-epoch>
next <invalid-block>
rpo
<upstream print_rpo output>
layout_print
<upstream print_block_layout output>
loops_print
<upstream print_loops output>
liveness_print
<upstream print_liveness output>
metadata <block-count>
block <id> <layout-index> <flags> <multiple-incoming> <has-phis>
end
```

Exactly one `live` or `ignored` record is emitted per local value index, in
ascending order through the declared maximum. Fresh ignored entries have epoch
zero; reused entries preserve the prior function's epoch. Loop begin/end and
liveness first/last are layout indices. `next` comes from the block lookup at
one past the final layout index. The four printer sections contain untouched
upstream output using TestIR block names. The driver checks that `print_rpo`
preserves both metadata fields for every current block; failure aborts in both
assertion configurations. Unreachable-block layout indices are normalized to
4294967295 when flags are zero, because only upstream assertion builds install
that sentinel. Flags remain exact.

Analysis cases must satisfy upstream Analyzer preconditions. Parser acceptance
does not validate dominance or all PHI/CFG consistency rules. Parser-accepted
malformed SSA belongs in `parse`, `records`, `print`, or `adaptor` cases, not in
an upstream analysis invocation that would assert or access invalid memory.

## Build contract

Compile through the shared `test-support/oracle.rs` helper, with include root
`reference/tpde/include`, additional flag `-stdlib=libc++`, and these sources:

```text
tests/oracle.cpp
reference/tpde/src/test/TestIR.cpp
reference/tpde/src/util/SmallVector.cpp
```

The helper supplies `-std=c++20 -O3`, adding `-DNDEBUG` for its release-assertion
configuration. The versioned `tpde-component` runner profile provides
`clang++-21` 21.1.5 and libc++/libc++abi 21. Exceptions remain enabled.
`TPDE_LOGGING` is unset; no spdlog, LLVM, Fadec, Disarm, native compiler source,
or generated encoder input is needed. Builds and execution run only on the runners.

## Registered execution roster

The default oracle marker is
`TPDE_TEST_IR_ORACLE PASS programs=174 comparisons=492 cpp_modes=2 active=0`.
The ASAN integration target compiles and runs the same Rust path and C++ oracle
with `-fsanitize=address,undefined -fno-sanitize-recover=all`; its marker ends
in `active=1`. The fixed cases comprise 27 analysis-admitted modules, seven
additional parser-accepted modules, and 43 rejected modules. Ten additional
linear CFG sizes run records/print/adaptor/analyze in each assertion mode.
These numbers describe the test contract, not a completed execution receipt.
The two activation controls are separate from the 492 semantic comparisons and
emit `TPDE_TEST_IR_SANITIZER_CONFIG PASS active=0 cpp_modes=2 checks=2`
(`active=1` for ASAN). Package `build.rs` derives the cfg from compiler-provided
`CARGO_CFG_SANITIZE`; the runner supplies actual Rust sanitizer instrumentation.

A second integration test launches `tpde-test-ir analyze` using Cargo's built
binary path and streams valid/malformed source through stdin. It compares the
actual textual analysis protocol and success/failure status and emits
`TPDE_TEST_IR_CLI PASS programs=2`. The binary is compiled as a normal package
target; the test does not run Cargo recursively.

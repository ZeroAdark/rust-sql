# TestIR component

Fresh Rust implementation of the complete pinned TPDE TestIR parser/model,
printer and adaptor. The component binary feeds runtime text through those
interfaces into the existing Rust Analyzer.

```text
tpde-test-ir analyze < program.testir
tpde-test-ir print < program.testir
```

The default mode is `analyze`; `parse`, `records`, and `adaptor` are also
available. Successful parsing prints `parse 1`; malformed syntax prints
`parse 0` and exits 1. Unknown modes exit 64. The analysis mode reports actual
block layout, loops and liveness. Parser acceptance alone does not establish
SSA dominance or PHI/CFG consistency; analysis inputs must satisfy the upstream
Analyzer's preconditions. Declarations and empty bodies are represented and
skipped before analysis.

```text
f(%a) {
entry:
br ^exit
exit:
ret %a
}
```

This is a parsing/analysis component, with no native code emission,
installation or execution. Original C++ is isolated under `reference/` for
independent comparisons; production does not call it. `tests/PROTOCOL.md`
documents the differential modes and actual reference build inputs.

Build and execute through the registered `tpde-component` runner profile only.
The package has library tests, ownership doctests, a pinned C++ differential
integration test, and a test that launches the real binary with runtime text.
Qualification and performance are separate, exact-source runner verdicts;
source completion is not a PASS.

# Fresh Fadec generator

`tpde_fadec_gen` rewrites the complete pinned Fadec `parseinstrs.py` generator
in Rust. Its production dependency is `data/instrs.txt`; it has no Python or C
generation dependency. Python lives only under `tests/` as an isolated oracle.

The reference revision is `3994d89500985491b1a7ccc17827a22058b3de49`.
`UPSTREAM.tsv` records pristine file identities; `tests/reference/tree.json`
records the upstream Git blob roster, and `NOTICE` retains the upstream license.
The instruction input contains 2,337 source rows, including 13 weak and 14
undocumented rows. These are lexical source counts, not execution evidence.

The command line retains the upstream positional arguments and flags:

```text
fadec-gen [--32] [--64] [--with-undoc] [--stats] \
  [--forms PATH] [--semantic PATH] \
  {decode,encode,encode2} TABLE OUT_PUBLIC OUT_PRIVATE
```

The two outputs are Rust source. Decode generation uses the requested architecture
root order; encode and encode2 preserve upstream's always-64-bit form admission,
regardless of architecture flags. Undocumented rows are verified before filtering.
`--stats` prints decoder statistics. `-` reads stdin or writes stdout.
`--forms` and `--semantic` are additive machine-readable evidence outputs.

The library accepts caller-owned instruction text and `Options`, and returns
owned Rust source, semantic records, source-form records and decoder statistics.
Generation errors return `Result::Err`; output files are opened only after
successful generation. CLI argument errors return status 2, generation/I/O errors
status 1. Error wording and Python exception classes are not an output contract.

This package is standalone cold build tooling. Its `Vec`, `String`, `BTreeMap`
and `BTreeSet` storage has ordinary ownership and destruction; it is never engine
query-lifetime data and never enters mcx arenas. Ordered containers provide
deterministic output without a randomized hash seed. This is the explicit std
collection justification required by repository policy. It makes no allocation,
no-drop engine, or hot-path performance claim.

The decoder output supplies tables and name data to a future Rust decoder. The
encode output supplies packed constants and the alternate table to a future Rust
encoder. The encode2 output supplies generated Rust functions and a monomorphized
runtime helper interface; the actual `encode2.c`/`fadec-enc2.h` runtime rewrite is
a separate dependency. See `generated-output-contract.md`. A shape-only compile
fixture establishes type compatibility, not instruction-byte correctness or
installed native execution.

All generator runs, builds, tests and generated-source compilation run on the
runners. Full qualification needs Python 3.9 or later and Rust 1.96 from the locked
runner image, feature `upstream-oracle`, and `FADEC_GEN_ARTIFACT_DIR` (or the
runner-provided `COMPONENT_ARTIFACT_DIR`) inside the durable artifact tree.
Missing tools are errors; no cases silently skip. Tests exercise 18 primary
generator/architecture/undocumented combinations plus six boundary profiles:
decoder roots in reverse order and encoder invocations with no architecture flags.
The actual pinned Python CLI runs for every profile. Nine additional pipeline
cases check duplicate roots and undocumented unknown-size admission.

All 16 decoder/encoder table profiles compile and execute their emitted Rust,
then compare observed constants, arrays, strings and roots with the Python output.
All encode2 profile contents are accounted for in a deduplicated execution map;
each distinct generated C/Rust body set executes an independently derived form
roster using matching recording helpers. Full scratch contents, helper arguments,
returns and generator-owned byte writes compare. Those helpers test the generator
contract; actual encoder-runtime byte equivalence remains a separate open gate.
Per-profile verdicts accumulate failures and preserve the executed artifacts.

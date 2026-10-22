# Pinned TestIR reference

These 15 files are byte-for-byte copies from TPDE revision
`116b2b83eb8491fd63c017285c91b007b8a26a67`. `manifest.json` lists every copied
path, SHA-256 digest, and Git blob ID. Each blob ID was checked against the
cached upstream Git tree for that exact revision before copying. Files retain
their upstream SPDX notices and the Apache 2.0 / LLVM exception license texts.

The source subset contains the complete TestIR parser, printer and adaptor,
the actual Analyzer template, and its transitive TPDE headers and SmallVector
implementation. It excludes native TestIR compiler backends and all encoder
submodules. No copied source is modified and no previous Rust TPDE source is
used. The independently written driver and transport are in `../tests`;
`../tests/PROTOCOL.md` documents the build inputs and output contract.

Reference code is compiled only for the test oracle. It is not linked into the
Rust library and provides no production execution fallback. C++ compilation
and runtime qualification remain runner-only.

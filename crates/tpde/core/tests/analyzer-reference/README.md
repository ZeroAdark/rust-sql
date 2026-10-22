Analyzer oracle
===============

The manifest pins the unchanged upstream Analyzer and its transitive headers and SmallVector implementation. oracle.cpp is an independent IR adaptor and transport over those real C++ implementations. The Rust integration test serializes 83 CFGs, with four highest-index/argument policies and both assertion modes, for 664 complete snapshots. The input protocol contains only CFG, instruction, value, PHI, and epoch data; no expected analysis results are sent.

Comparison includes layout, loop parents/ranges/counts/definition counters, all liveness fields and stale epochs, all diagnostic printers, and block metadata. Unreachable block primary metadata is normalized to UINT_MAX because upstream assigns that sentinel only with assertions. print_rpo must preserve all original metadata in each implementation. The secondary flags remain compared exactly. Cases with no nonignored values and no highest index are checked only in Rust unit tests because upstream diagnostic printing reads an absent liveness entry.

The C++ driver compiles with clang C++20, -O3, and -stdlib=libc++; no copied prior Rust implementation or handcoded analysis oracle is used.

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

// Pinned C++ CompilerA64 reference leg for the raw-operation paired benchmark.
//
// Reuses the differential oracle's RawCompiler and AAPCS driver verbatim by
// including raw_oracle.cpp with its `main` renamed, so the measured reference
// is exactly the qualified oracle code compiled with the ndebug reference
// flags. Protocol (stdin, all little-endian u64): iterations, then the
// ordinary raw protocol body (mode, operation count, operations). Every
// iteration re-parses the trivial IR, constructs a fresh RawCompiler, runs the
// script and exports the object (mode 0), or replays the AAPCS assigner script
// (mode 1); protocol output bytes are redirected into memory and consumed by
// an FNV-1a checksum so both sides do the same work.
#define main raw_oracle_main
#include "raw_oracle.cpp"
#undef main
#include <sstream>
#include "perf.hpp"

int main() {
  const uint64_t iterations = number(), mode = number(), count = number();
  std::vector<Operation> operations;
  for (uint64_t index = 0; index < count; ++index) {
    Operation operation{number(), {}};
    for (auto &value : operation.args) value = number();
    operations.push_back(operation);
  }
  if (mode > 1) return 2;
  std::stringbuf sink;
  auto *console = std::cout.rdbuf(&sink);
  unsigned long long checksum = 0xcbf29ce484222325ULL;
  size_t produced = 0;
  auto once = [&] {
    sink.str("");
    if (mode == 1) {
      abi(operations);
    } else {
      test::TestIR ir;
      if (!ir.parse_ir("raw() {\nentry:\nterminate\n}\n")) std::abort();
      test::TestIRAdaptor adaptor{&ir};
      RawCompiler compiler{&adaptor, operations};
      if (!compiler.compile()) std::abort();
      const auto object = compiler.assembler.build_object_file();
      put(object.size());
      std::cout.write(reinterpret_cast<const char *>(object.data()), object.size());
    }
    const auto bytes = sink.view();
    checksum = tpde_bench::fnv1a(checksum, reinterpret_cast<const unsigned char *>(bytes.data()), bytes.size());
    produced = bytes.size();
  };
  tpde_bench::Counter counter;
  for (uint64_t index = 0; index < iterations / 10 + 1; ++index) once();
  checksum = 0xcbf29ce484222325ULL;
  const auto leg = counter.measure([&] {
    for (uint64_t index = 0; index < iterations; ++index) once();
  });
  std::cout.rdbuf(console);
  char extra[160];
  std::snprintf(extra, sizeof extra, "checksum=%016llx output_size=%zu iterations=%llu", checksum, produced,
                static_cast<unsigned long long>(iterations));
  tpde_bench::put_measurement(mode == 1 ? "abi" : "raw", leg, extra);
  std::printf("maxrss %llu\n", tpde_bench::peak_rss_kib());
  std::printf("pmu %s\n", counter.unavailable.empty() ? "ok" : counter.unavailable.c_str());
  return 0;
}

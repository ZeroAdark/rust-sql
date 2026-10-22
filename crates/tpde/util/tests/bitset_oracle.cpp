// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <cassert>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <memory>
#include <utility>
#include "tpde/util/SmallBitSet.hpp"

static uint64_t read(unsigned bytes) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) {
    int byte = std::getchar();
    if (byte == EOF) std::abort();
    value |= uint64_t(byte) << (i * 8);
  }
  return value;
}
static void write(uint64_t value) {
  for (unsigned i = 0; i < 8; ++i) std::putchar(value >> (i * 8));
}

template <unsigned Bits>
static void snapshot(tpde::util::SmallBitSet<Bits> &set) {
  write(set.bit_size);
  write(set.data.size());
  const auto first = set.first_set();
  write(first ? *first : UINT64_MAX);
  for (size_t word = 0; word < set.data.size(); ++word) {
    write(set.data[word]);
    uint64_t observed = 0;
    for (unsigned bit = 0; bit < 64; ++bit) {
      observed |= uint64_t(set.is_set(word * 64 + bit)) << bit;
    }
    write(observed);
  }
}

template <unsigned Bits>
static void run(unsigned operations) {
  using Set = tpde::util::SmallBitSet<Bits>;
  std::unique_ptr<Set> sets[] = {std::make_unique<Set>(), std::make_unique<Set>()};
  snapshot(*sets[0]);
  snapshot(*sets[1]);
  for (unsigned i = 0; i < operations; ++i) {
    const auto op = read(1), a = read(1), b = read(1);
    read(1);
    const auto n = read(4), value = read(8);
    auto &set = *sets[a];
    switch (op) {
    case 0: break;
    case 1: set.clear(); break;
    case 2: set.resize(n); break;
    case 3: set.zero(); break;
    case 4: set.push_back(n != 0); break;
    case 5: set.mark_set(n); break;
    case 6: set.mark_unset(n); break;
    case 7: {
      auto moved = std::make_unique<Set>(std::move(*sets[b]));
      sets[a] = std::move(moved);
      break;
    }
    case 8: set = std::move(*sets[b]); break;
    case 9: set = std::move(set); break;
    case 10: set.data[n] = value; break;
    default: std::abort();
    }
    snapshot(*sets[0]);
    snapshot(*sets[1]);
  }
}
int main() {
  const auto words = read(1), operations = read(4);
  switch (words) {
  case 0: run<0>(operations); break;
  case 1: run<64>(operations); break;
  case 4: run<256>(operations); break;
  default: std::abort();
  }
}

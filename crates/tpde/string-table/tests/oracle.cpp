// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include "tpde/StringTable.hpp"
#include <cstdint>
#include <iostream>
#include <string>

static uint32_t read32() {
  uint32_t result = 0;
  for (unsigned i = 0; i < 4; ++i) {
    int byte = std::cin.get();
    if (byte < 0) throw std::runtime_error("truncated input");
    result |= uint32_t(byte) << (8 * i);
  }
  return result;
}

static void write64(uint64_t value) {
  for (unsigned i = 0; i < 8; ++i) std::cout.put(char(value >> (8 * i)));
}

int main() {
  tpde::StringTable table;
  write64(0);
  write64(table.size());
  std::cout.write(table.data(), table.size());
  for (int kind; (kind = std::cin.get()) != -1;) {
    const uint32_t prefix_len = read32();
    const uint32_t bytes_len = read32();
    std::string prefix(prefix_len, '\0'), bytes(bytes_len, '\0');
    std::cin.read(prefix.data(), prefix.size());
    std::cin.read(bytes.data(), bytes.size());
    if (!std::cin || (kind != 0 && kind != 1)) return 2;
    const size_t offset = kind == 0 ? table.add(bytes) : table.add_prefix(prefix, bytes);
    write64(offset);
    write64(table.size());
    std::cout.write(table.data(), table.size());
  }
  return std::cout ? 0 : 3;
}

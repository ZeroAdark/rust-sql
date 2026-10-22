// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include "tpde/ValLocalIdx.hpp"
#include <cstdio>
#include <initializer_list>
#include <type_traits>

static void out(uint64_t value) {
  for (unsigned i = 0; i != 8; ++i) std::putchar(value >> (8 * i));
}

int main() {
  static_assert(!std::is_convertible_v<uint32_t, tpde::ValLocalIdx>);
  static_assert(!std::is_convertible_v<tpde::ValLocalIdx, uint32_t>);
  out(sizeof(tpde::ValLocalIdx));
  out(alignof(tpde::ValLocalIdx));
  for (unsigned bit = 0; bit != 32; ++bit) {
    for (const uint32_t value : {uint32_t(1) << bit, ~(uint32_t(1) << bit)}) {
      const auto index = static_cast<tpde::ValLocalIdx>(value);
      out(static_cast<uint32_t>(index));
      out(index == static_cast<tpde::ValLocalIdx>(value));
      out(index < static_cast<tpde::ValLocalIdx>(0x80000000u));
    }
  }
  out(static_cast<uint32_t>(static_cast<tpde::ValLocalIdx>(0)));
  out(static_cast<uint32_t>(static_cast<tpde::ValLocalIdx>(UINT32_MAX)));
}

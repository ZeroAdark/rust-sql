// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include "tpde/util/AddressSanitizer.hpp"
#include <cstdio>
#include <cstring>

#if defined(TPDE_ASAN_BUILD)
extern "C" int __asan_address_is_poisoned(void const volatile *pointer);
#endif

int main(int argc, char **argv) {
  alignas(64) unsigned char region[64];
  std::memset(region, 0xa5, sizeof(region));
  if (argc == 2) {
    const bool recovered = std::strcmp(argv[1], "recovered") == 0;
    if (!recovered && std::strcmp(argv[1], "poisoned") != 0)
      return 2;
    tpde::util::poison_memory_region(region + 16, 8);
    if (recovered)
      tpde::util::unpoison_memory_region(region + 16, 8);
    std::fputs("TPDE_ASAN_ACCESS armed=1\n", stderr);
    const auto value = *static_cast<volatile unsigned char *>(region + 16);
    if (value != 0xa5)
      return 3;
    tpde::util::unpoison_memory_region(region, sizeof(region));
    std::fputs("TPDE_ASAN_ACCESS survived=1\n", stderr);
    return 0;
  }
  std::putchar(tpde::util::address_sanitizer_active);
  unsigned char command[3];
  while (std::fread(command, 1, 3, stdin) == 3) {
    if (command[1] + command[2] > sizeof(region))
      return 4;
    if (command[0] == 0)
      tpde::util::poison_memory_region(region + command[1], command[2]);
    else if (command[0] == 1)
      tpde::util::unpoison_memory_region(region + command[1], command[2]);
    else
      return 5;
    for (unsigned i = 0; i < sizeof(region); ++i) {
#if defined(TPDE_ASAN_BUILD)
      std::putchar(__asan_address_is_poisoned(region + i) != 0);
#else
      std::putchar(0);
#endif
    }
  }
  tpde::util::unpoison_memory_region(region, sizeof(region));
  for (auto byte : region)
    if (byte != 0xa5)
      return 6;
  return 0;
}

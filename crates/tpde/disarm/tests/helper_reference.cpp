// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#include "disarm64.h"
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>

static uint64_t read64(const unsigned char *p) {
  uint64_t result = 0;
  for (unsigned i = 0; i < 8; ++i)
    result |= uint64_t(p[i]) << (8 * i);
  return result;
}

static void write_word(uint64_t value, unsigned bytes) {
  unsigned char out[8];
  for (unsigned i = 0; i < bytes; ++i)
    out[i] = value >> (8 * i);
  if (std::fwrite(out, 1, bytes, stdout) != bytes)
    std::abort();
}

int main() {
  unsigned char row[12];
  for (;;) {
    size_t count = std::fread(row, 1, sizeof(row), stdin);
    if (!count)
      return std::ferror(stdin) ? 1 : 0;
    if (count != sizeof(row) || row[2] || row[3])
      return 2;
    uint64_t value = read64(row + 4), result = 0;
    switch (row[0]) {
    case 1:
      if (!row[1] || row[1] >= 64)
        return 3;
      result = da_sext(int64_t(value), row[1]);
      break;
    case 2:
      result = da_immadd(int64_t(value));
      break;
    case 3:
      result = da_immlogical(value, row[1]);
      break;
    case 4: {
      uint32_t bits = uint32_t(value);
      float number;
      std::memcpy(&number, &bits, sizeof(number));
      result = da_immfmov32(number);
      break;
    }
    case 5: {
      double number;
      std::memcpy(&number, &value, sizeof(number));
      result = da_immfmov64(number);
      break;
    }
    case 6:
      result = da_immsimdmovi(value);
      break;
    case 7: {
      if (row[1] > 31)
        return 4;
      constexpr uint32_t canary = 0xa5c3f19d;
      uint32_t words[6] = {canary, canary, canary, canary, canary, canary};
      unsigned length = de64_MOVconst(words + 1, DA_GReg{uint8_t(row[1])}, value);
      if (!length || length > 4 || words[0] != canary || words[5] != canary)
        return 5;
      for (unsigned i = length + 1; i < 5; ++i)
        if (words[i] != canary)
          return 6;
      write_word(length, 4);
      for (unsigned i = 1; i < 5; ++i)
        write_word(words[i], 4);
      continue;
    }
    default:
      return 7;
    }
    write_word(result, 8);
  }
}

// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include "disarm64.h"
static uint64_t get(const unsigned char* p, unsigned count) {
  uint64_t result = 0;
  for (unsigned i = 0; i < count; ++i) result |= uint64_t(p[i]) << (i*8);
  return result;
}
static void text(const Da64Inst& inst, uint64_t addr, bool absolute) {
  char buffer[160];
  std::memset(buffer, 0x5a, sizeof(buffer));
  if (absolute) da64_format_abs(&inst, addr, buffer+16);
  else da64_format(&inst, buffer+16);
  for (unsigned i = 0; i < 16; ++i)
    if (buffer[i] != 0x5a || buffer[144+i] != 0x5a) std::abort();
  const size_t size = std::strlen(buffer+16);
  if (size >= 128) std::abort();
  std::putchar(size & 255);
  std::putchar(size >> 8);
  std::fwrite(buffer+16, 1, size+1, stdout);
}
int main() {
  unsigned char row[38];
  size_t size;
  while ((size = std::fread(row, 1, sizeof(row), stdin)) != 0) {
    if (size != sizeof(row)) return 2;
    Da64Inst inst = {};
    inst.mnem = get(row, 2);
    for (unsigned i = 0; i < 5; ++i) {
      const unsigned char* src = row+2+i*4;
      inst.ops[i].type = src[0];
      inst.ops[i].reg = src[1];
      inst.ops[i].uimm16 = get(src+2, 2);
    }
    inst.imm64 = get(row+22, 8);
    text(inst, get(row+30, 8), true);
    text(inst, 0, false);
  }
  return std::ferror(stdin) || std::fflush(stdout) != 0 ? 3 : 0;
}

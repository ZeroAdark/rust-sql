// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include "disarm64.h"
static_assert(sizeof(Da64Op) == 4 && sizeof(Da64Inst) == 32);
static void put(uint64_t value, unsigned bytes) {
  for (unsigned i = 0; i < bytes; ++i) std::putchar(value >> (8*i));
}
static uint64_t get(const unsigned char* input, unsigned bytes) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) value |= uint64_t(input[i]) << (8*i);
  return value;
}
static uint16_t data(const Da64Op& op) {
  switch (op.type) {
  case DA_OP_REGGP: case DA_OP_REGGPINC: case DA_OP_REGSP: return op.reggp.sf;
  case DA_OP_REGGPEXT: return op.reggpext.sf | op.reggpext.ext << 1 | op.reggpext.shift << 8;
  case DA_OP_REGFP: return op.regfp.size;
  case DA_OP_REGVEC: return op.regvec.va;
  case DA_OP_REGVIDX: return op.regvidx.esize | op.regvidx.elem << 8;
  case DA_OP_REGVTBL: return op.regvtbl.va | op.regvtbl.cnt << 8;
  case DA_OP_REGVTBLIDX: return op.regvtblidx.esize | op.regvtblidx.elem << 4 | op.regvtblidx.cnt << 8;
  case DA_OP_MEMREG: case DA_OP_MEMREGPOST: return op.memreg.sc | op.memreg.ext << 1 | op.memreg.shift << 5 | op.memreg.offreg << 8;
  case DA_OP_COND: return op.cond;
  default: return op.uimm16;
  }
}
static void format(const Da64Inst& inst, uint64_t address) {
  char buffer[160];
  std::memset(buffer, 0x5a, sizeof(buffer));
  da64_format_abs(&inst, address, buffer + 16);
  for (unsigned i = 0; i < 16; ++i)
    if (buffer[i] != 0x5a || buffer[144+i] != 0x5a) std::abort();
  const size_t length = std::strlen(buffer+16);
  if (length >= 128) std::abort();
  put(length, 2);
  std::fwrite(buffer+16, 1, length+1, stdout);
}
int main() {
  unsigned char record[20];
  size_t size;
  while ((size = std::fread(record, 1, sizeof(record), stdin)) != 0) {
    if (size != sizeof(record)) return 2;
    const uint32_t word = get(record, 4);
    Da64Inst inst = {};
    inst.imm64 = get(record+4, 8);
    da64_decode(word, &inst);
    put(da64_classify(word), 4);
    put(inst.mnem, 2);
    for (const Da64Op& op : inst.ops) { put(op.type, 1); put(op.reg, 1); put(data(op), 2); }
    put(inst.imm64, 8);
    format(inst, get(record+12, 8));
    char zero[128];
    std::memset(zero, 0x5a, sizeof(zero));
    da64_format(&inst, zero);
    const size_t length = std::strlen(zero);
    if (length >= sizeof(zero)) return 3;
    put(length, 2);
    std::fwrite(zero, 1, length+1, stdout);
  }
  return std::ferror(stdin) || std::fflush(stdout) != 0 ? 4 : 0;
}

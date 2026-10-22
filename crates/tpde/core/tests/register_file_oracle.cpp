// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include "tpde/RegisterFile.hpp"

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <iostream>

using namespace tpde;

static_assert(sizeof(RegisterFile<0, 1>) == 32);

static void out(u64 value) {
  for (unsigned i = 0; i < 8; ++i) std::putchar(u8(value >> (i * 8)));
}

template <unsigned B, unsigned R>
static void snapshot(const RegisterFile<B, R> &file, u64 assigned, u64 result) {
  out(result);
  out(file.allocatable);
  out(file.used);
  out(file.clobbered);
  for (u8 clock : file.clocks) out(clock);
  out(__builtin_popcountll(file.used));
  for (auto id : file.used_regs()) out(id);
  for (unsigned id = 0; id < B * R; ++id) {
    Reg reg{id};
    out(file.is_used(reg));
    out(file.is_fixed(reg));
    out(file.is_clobbered(reg));
    out(file.lock_counts[id]);
    out(file.reg_bank(reg).id());
    if (assigned & (1ull << id)) {
      out(static_cast<u32>(file.assignments[id].local_idx));
      out(file.assignments[id].part);
    }
    if (file.is_used(reg)) {
      out(static_cast<u32>(file.reg_local_idx(reg)));
      out(file.reg_part(reg));
    }
  }
  if constexpr (R < 64) {
    for (u8 bank_id : {0, 1}) {
      RegBank bank{bank_id};
      out(file.bank_regs(bank));
      for (u64 exclusion : std::array<u64, 6>{0, UINT64_MAX, 0x5555555555555555ull, 0xaaaaaaaaaaaaaaaaull, file.used, ~file.used}) {
        out(file.find_first_free_excluding(bank, exclusion).id());
        out(file.find_first_nonfixed_excluding(bank, exclusion).id());
      }
    }
  }
}

template <unsigned B, unsigned R>
static void run(unsigned count) {
  RegisterFile<B, R> file;
  u64 assigned = 0;
  out(sizeof(file));
  out(alignof(decltype(file)));
  snapshot(file, assigned, 256);
  for (unsigned i = 0; i < count; ++i) {
    unsigned code;
    u64 a, b, c, result = 256;
    if (!(std::cin >> code >> a >> b >> c)) std::abort();
    switch (code) {
    case 0: file.reset(); break;
    case 1: file.allocatable = a; break;
    case 2:
      file.mark_used(Reg{a}, ValLocalIdx(u32(b)), u32(c));
      assigned |= 1ull << a;
      break;
    case 3: file.update_reg_assignment(Reg{a}, ValLocalIdx(u32(b)), u32(c)); break;
    case 4: file.unmark_used(Reg{a}); break;
    case 5: file.mark_fixed(Reg{a}); break;
    case 6: file.unmark_fixed(Reg{a}); break;
    case 7: file.inc_lock_count(Reg{a}); break;
    case 8: result = file.dec_lock_count(Reg{a}); break;
    case 9: file.dec_lock_count_must_zero(Reg{a}, u8(b)); break;
    case 10: file.mark_clobbered(Reg{a}); break;
    case 11: file.clocks[a] = u8(b); break;
    default: std::abort();
    }
    snapshot(file, assigned, result);
  }
}

int main() {
  out(sizeof(Reg));
  out(alignof(Reg));
  out(sizeof(RegBank));
  out(alignof(RegBank));
  out(sizeof(RegisterFile<2, 32>::Assignment));
  out(alignof(RegisterFile<2, 32>::Assignment));
  out(RegBank{}.id());
  for (unsigned id = 0; id <= 255; ++id) {
    Reg reg{id};
    out(reg.id());
    out(reg.invalid());
    out(reg.valid());
    out(reg == Reg::make_invalid());
    out(RegBank{u8(id)}.id());
    out(RegBank{u8(id)} == RegBank{});
    out(RegisterFile<2, 32>::reg_bank(reg).id());
  }
  unsigned banks, regs, count;
  while (std::cin >> banks >> regs >> count) {
    if (banks == 1 && regs == 1) run<1, 1>(count);
    else if (banks == 2 && regs == 1) run<2, 1>(count);
    else if (banks == 1 && regs == 32) run<1, 32>(count);
    else if (banks == 2 && regs == 32) run<2, 32>(count);
    else if (banks == 4 && regs == 16) run<4, 16>(count);
    else if (banks == 1 && regs == 64) run<1, 64>(count);
    else std::abort();
  }
}

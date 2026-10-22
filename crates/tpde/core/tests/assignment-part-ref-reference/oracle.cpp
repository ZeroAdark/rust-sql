// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <algorithm>
#include <array>
#include <bit>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iterator>
#include <limits>
#include <memory>
#include <new>
#include <type_traits>
#include <utility>

#include "tpde/AssignmentPartRef.hpp"

namespace {
void require(bool condition) {
  if (!condition) std::abort();
}

void initialize(tpde::ValueAssignment *value, uint32_t count) {
  std::memset(value, 0, offsetof(tpde::ValueAssignment, parts) +
                            count * sizeof(tpde::ValueAssignment::Part));
  value->frame_off = -0x1234567;
  value->part_count = count;
  value->references_left = 0x98765432;
  value->max_part_size = 128;
  value->pending_free = true;
  value->variable_ref = false;
  value->stack_variable = true;
  value->delay_free = true;
}

void exhaustive() {
  tpde::AssignmentAllocator allocator;
  auto *value = allocator.allocate(3);
  initialize(value, 3);
  value->parts[0] = 0xc369;
  value->parts[2] = 0x96ac;
  tpde::AssignmentPartRef ref(value, 1);
  std::array<unsigned char, offsetof(tpde::ValueAssignment, parts)> header{};
  std::memcpy(header.data(), value, header.size());
  for (uint32_t raw = 0; raw <= 0xffff; ++raw) {
    value->parts[1] = static_cast<uint16_t>(raw);
    require(ref.assignment() == value);
    std::array<unsigned char, 140> row{};
    size_t cursor = 0;
    auto u16 = [&](uint16_t word) {
      row[cursor++] = static_cast<unsigned char>(word);
      row[cursor++] = static_cast<unsigned char>(word >> 8);
    };
    auto u32 = [&](uint32_t word) {
      u16(static_cast<uint16_t>(word));
      u16(static_cast<uint16_t>(word >> 16));
    };
    u32(ref.bank().id());
    u32(ref.get_reg().id());
    u32(ref.modified());
    u32(ref.fixed_assignment());
    u32(ref.register_valid());
    u32(ref.stack_valid());
    u32(ref.part_size());
    auto mutate = [&](auto operation) {
      value->parts[1] = static_cast<uint16_t>(raw);
      operation();
      require(ref.assignment() == value);
      require(std::memcmp(header.data(), value, header.size()) == 0);
      require(value->parts[0] == 0xc369 && value->parts[2] == 0x96ac);
      u16(value->parts[1]);
    };
    mutate([&] { ref.reset(); });
    for (uint8_t bank = 0; bank < 8; ++bank)
      mutate([&] { ref.set_bank(tpde::RegBank(bank)); });
    for (uint32_t low = 0; low < 32; ++low)
      mutate([&] { ref.set_reg(tpde::Reg(((raw >> 5) & 7) * 32 + low)); });
    for (bool flag : {false, true})
      mutate([&] { ref.set_modified(flag); });
    for (bool flag : {false, true})
      mutate([&] { ref.set_fixed_assignment(flag); });
    for (bool flag : {false, true})
      mutate([&] { ref.set_register_valid(flag); });
    mutate([&] { ref.set_stack_valid(); });
    for (uint32_t shift = 0; shift < 8; ++shift)
      mutate([&] { ref.set_part_size(uint32_t{1} << shift); });
    require(cursor == row.size());
    require(std::fwrite(row.data(), 1, row.size(), stdout) == row.size());
  }
  allocator.deallocate(value);
}

volatile uint32_t observed;

void control(const char *name, bool bad) {
  tpde::AssignmentAllocator allocator;
  auto *value = allocator.allocate(3);
  initialize(value, 3);
  value->parts[0] = 0x41;
  value->parts[1] = 0x41;
  uint32_t index = 0;
  if (std::strcmp(name, "stack-part") == 0 ||
      std::strcmp(name, "custom-part") == 0)
    index = bad ? 1 : 0;
  tpde::AssignmentPartRef ref(value, index);
  if (std::strcmp(name, "bank") == 0) {
    ref.set_bank(tpde::RegBank(bad ? 8 : 7));
  } else if (std::strcmp(name, "reg") == 0) {
    ref.set_reg(tpde::Reg(bad ? 33 : 65));
  } else if (std::strcmp(name, "size-power") == 0) {
    ref.set_part_size(bad ? 3 : 4);
  } else if (std::strcmp(name, "size-range") == 0) {
    ref.set_part_size(bad ? 256 : 128);
  } else if (std::strcmp(name, "frame-variable") == 0) {
    value->variable_ref = bad;
    observed = static_cast<uint32_t>(ref.frame_off());
  } else if (std::strcmp(name, "frame-zero") == 0) {
    value->frame_off = bad ? 0 : -128;
    observed = static_cast<uint32_t>(ref.frame_off());
  } else if (std::strcmp(name, "stack-nonref") == 0) {
    value->variable_ref = !bad;
    value->stack_variable = true;
    observed = static_cast<uint32_t>(ref.variable_stack_off());
  } else if (std::strcmp(name, "stack-nonstack") == 0) {
    value->variable_ref = true;
    value->stack_variable = !bad;
    observed = static_cast<uint32_t>(ref.variable_stack_off());
  } else if (std::strcmp(name, "stack-part") == 0) {
    value->variable_ref = true;
    value->stack_variable = true;
    observed = static_cast<uint32_t>(ref.variable_stack_off());
  } else if (std::strcmp(name, "custom-nonref") == 0) {
    value->variable_ref = !bad;
    value->stack_variable = false;
    value->var_ref_custom_idx = 0xfeed0001;
    observed = ref.variable_ref_data();
  } else if (std::strcmp(name, "custom-stack") == 0) {
    value->variable_ref = true;
    value->stack_variable = bad;
    value->var_ref_custom_idx = 0xfeed0001;
    observed = ref.variable_ref_data();
  } else if (std::strcmp(name, "custom-part") == 0) {
    value->variable_ref = true;
    value->stack_variable = false;
    value->var_ref_custom_idx = 0xfeed0001;
    observed = ref.variable_ref_data();
  } else {
    std::abort();
  }
  allocator.deallocate(value);
  std::puts("OK");
}

void zero_size() {
  tpde::AssignmentAllocator allocator;
  auto *value = allocator.allocate(1);
  initialize(value, 1);
  tpde::AssignmentPartRef ref(value, 0);
  ref.set_part_size(0);
  allocator.deallocate(value);
}

void offsets() {
  constexpr std::array<uint32_t, 12> indices{
      0, 1, 2, 3, 255, 256, 65535, 65536, 16843009, 16843010,
      2147483648u, 4294967295u};
  constexpr std::array<int32_t, 6> frames{
      1, -1, std::numeric_limits<int32_t>::min(),
      std::numeric_limits<int32_t>::max(), -4096, 4096};
  tpde::AssignmentAllocator allocator;
  auto *value = allocator.allocate(3);
  initialize(value, 3);
  value->variable_ref = false;
  value->stack_variable = false;
  for (uint32_t maximum = 0; maximum <= 255; ++maximum) {
    value->max_part_size = static_cast<uint8_t>(maximum);
    for (uint32_t index : indices) {
      for (int32_t frame : frames) {
        value->frame_off = frame;
        tpde::AssignmentPartRef ref(value, index);
        require(ref.assignment() == value);
        std::printf("O %u %u %d %u %d\n", maximum, index, frame,
                    ref.part_off(), ref.frame_off());
      }
    }
  }
  for (bool variable : {false, true}) {
    for (bool stack : {false, true}) {
      for (int32_t frame : frames) {
        value->variable_ref = variable;
        value->stack_variable = stack;
        value->frame_off = frame;
        tpde::AssignmentPartRef ref(value, 0);
        uint32_t result = variable
            ? (stack ? static_cast<uint32_t>(ref.variable_stack_off())
                     : ref.variable_ref_data())
            : static_cast<uint32_t>(ref.frame_off());
        std::printf("H %u %u %d %u %u %u\n", unsigned(variable), unsigned(stack),
                    frame, unsigned(ref.variable_ref()),
                    unsigned(ref.is_stack_variable()), result);
      }
    }
  }
  allocator.deallocate(value);
}

void reuse() {
  tpde::AssignmentAllocator allocator;
  uintptr_t previous = 0;
  for (uint32_t epoch = 0; epoch < 3; ++epoch) {
    auto *value = allocator.allocate(3);
    initialize(value, 3);
    value->frame_off = -128;
    value->references_left = 17;
    value->max_part_size = 16;
    value->pending_free = false;
    value->variable_ref = false;
    value->stack_variable = false;
    value->delay_free = false;
    value->parts[0] = 0x1234;
    value->parts[1] = 0xabcd;
    value->parts[2] = 0x55aa;
    const auto address = reinterpret_cast<uintptr_t>(value);
    std::printf("P %u %u\n", epoch, unsigned(address == previous));
    previous = address;
    {
      tpde::AssignmentPartRef first(value, 1);
      tpde::AssignmentPartRef other(value, 1);
      auto snapshot = [&](uint32_t step) {
        require(first.assignment() == value && other.assignment() == value);
        auto *owner = other.assignment();
        uint32_t frame;
        std::memcpy(&frame, owner, sizeof(frame));
        const uint32_t normal_frame = other.variable_ref()
            ? 0 : static_cast<uint32_t>(other.frame_off());
        std::printf("R %u %u %u %u %u %u %u %u %u %u %u %u\n", epoch, step,
                    unsigned(owner->parts[0]), unsigned(owner->parts[1]),
                    unsigned(owner->parts[2]), frame,
                    unsigned(owner->max_part_size), owner->references_left,
                    unsigned(other.variable_ref()),
                    unsigned(other.is_stack_variable()), other.part_off(),
                    normal_frame);
      };
      snapshot(0);
      first.reset();
      snapshot(1);
      first.set_bank(tpde::RegBank(3));
      snapshot(2);
      first.set_reg(tpde::Reg(109));
      snapshot(3);
      first.set_part_size(32);
      snapshot(4);
      first.set_register_valid(true);
      snapshot(5);
      first.set_fixed_assignment(true);
      snapshot(6);
      first.set_stack_valid();
      snapshot(7);
      first.set_modified(true);
      snapshot(8);
      first.set_register_valid(false);
      snapshot(9);
      value->frame_off = std::numeric_limits<int32_t>::max();
      value->max_part_size = 255;
      value->references_left = 37;
      snapshot(10);
      value->variable_ref = true;
      value->stack_variable = false;
      value->var_ref_custom_idx = 0xf1234567;
      snapshot(11);
      value->stack_variable = true;
      value->frame_off = std::numeric_limits<int32_t>::min();
      snapshot(12);
      value->variable_ref = false;
      value->stack_variable = false;
      value->frame_off = 1;
      snapshot(13);
    }
    allocator.deallocate(value);
    if (epoch == 1) allocator.reset();
  }
}
}

int main(int argc, char **argv) {
  require(argc >= 2);
  if (std::strcmp(argv[1], "exhaustive") == 0) {
    require(argc == 2);
    exhaustive();
  } else if (std::strcmp(argv[1], "control") == 0) {
    require(argc == 4);
    require(std::strcmp(argv[3], "good") == 0 ||
            std::strcmp(argv[3], "bad") == 0);
    control(argv[2], std::strcmp(argv[3], "bad") == 0);
  } else if (std::strcmp(argv[1], "zero-size") == 0) {
    require(argc == 2);
    zero_size();
  } else if (std::strcmp(argv[1], "offsets") == 0) {
    require(argc == 2);
    offsets();
  } else if (std::strcmp(argv[1], "reuse") == 0) {
    require(argc == 2);
    reuse();
  } else {
    std::abort();
  }
  require(std::fflush(stdout) == 0);
}

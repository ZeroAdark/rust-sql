// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <array>
#include <bit>
#include <cassert>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iterator>
#include <memory>
#include <new>
#include <type_traits>
#include <utility>

#include "tpde/src/ValueAssignment.cpp"

namespace {
struct Slab { void *data; size_t size; bool live; };
Slab slabs[65536];
size_t slab_count, live_slabs, live_bytes;
void require(bool condition) { if (!condition) std::abort(); }
size_t locate(const void *data) {
  const auto address = reinterpret_cast<uintptr_t>(data);
  for (size_t i = 0; i < slab_count; ++i) {
    auto begin = reinterpret_cast<uintptr_t>(slabs[i].data);
    if (slabs[i].live && address >= begin && address - begin < slabs[i].size)
      return i;
  }
  std::abort();
}
void release(void *data) {
  if (!data) return;
  size_t id = locate(data);
  require(data == slabs[id].data);
  slabs[id].live = false;
  --live_slabs;
  live_bytes -= slabs[id].size;
  std::free(data);
}
uint16_t part(uint32_t seed, uint32_t index) {
  return uint16_t(seed + index * uint32_t{31337});
}
uint32_t bits(const void *data) {
  uint32_t result;
  std::memcpy(&result, data, sizeof(result));
  return result;
}
void snapshot(char operation, size_t slot, const tpde::ValueAssignment *value) {
  const size_t id = locate(value);
  const size_t offset = reinterpret_cast<uintptr_t>(value) -
                        reinterpret_cast<uintptr_t>(slabs[id].data);
  uint64_t checksum = 0;
  for (uint32_t i = 0; i < value->part_count; ++i)
    checksum = checksum * uint64_t{131} + value->parts[i];
  const auto flags = unsigned(value->pending_free) |
      (unsigned(value->variable_ref) << 1) |
      (unsigned(value->stack_variable) << 2) | (unsigned(value->delay_free) << 3);
  const uint32_t size = value->variable_ref ? 0 : value->size();
  std::printf("%c %zu %zu %zu %zu %zu %zu %u %u %u %u %u %u %llu\n",
      operation, slot, id, offset, slabs[id].size, live_slabs, live_bytes,
      bits(&value->frame_off), value->part_count, bits(&value->references_left),
      unsigned(value->max_part_size), flags, size,
      static_cast<unsigned long long>(checksum));
}
void zero_snapshot(const tpde::ValueAssignment *value) {
  std::printf("Z %u %u %u %u %u %u\n", bits(&value->frame_off),
      value->part_count, bits(&value->references_left), unsigned(value->max_part_size),
      unsigned(value->pending_free) | (unsigned(value->variable_ref) << 1) |
      (unsigned(value->stack_variable) << 2) | (unsigned(value->delay_free) << 3),
      unsigned(value->first_part));
}
}
void *operator new(size_t size, std::align_val_t alignment) {
  void *data = nullptr;
  if (posix_memalign(&data, size_t(alignment), size) != 0) throw std::bad_alloc();
  require(slab_count < std::size(slabs));
  slabs[slab_count++] = {data, size, true};
  ++live_slabs;
  live_bytes += size;
  return data;
}
void operator delete(void *data, std::align_val_t) noexcept { release(data); }
void operator delete(void *data, size_t size, std::align_val_t) noexcept {
  if (data) require(slabs[locate(data)].size == size);
  release(data);
}
#ifdef TPDE_ASAN_BUILD
extern "C" int __asan_address_is_poisoned(void const volatile *address);
#endif

int main() {
  std::array<tpde::ValueAssignment *, 128> slots{};
  std::array<const unsigned char *, 128> retired{};
  {
    tpde::AssignmentAllocator allocator;
    char op;
    while (std::scanf(" %c", &op) == 1) {
      switch (op) {
      case 'L':
        std::printf("L %zu %zu %zu %zu %zu %zu %zu %u\n",
            sizeof(tpde::ValueAssignment), alignof(tpde::ValueAssignment),
            offsetof(tpde::ValueAssignment, frame_off),
            offsetof(tpde::ValueAssignment, part_count),
            offsetof(tpde::ValueAssignment, references_left),
            offsetof(tpde::ValueAssignment, max_part_size),
            offsetof(tpde::ValueAssignment, parts),
            tpde::AssignmentAllocator::NumPartsIncluded);
        break;
      case 'I':
      case 'U': {
        uint32_t count;
        require(std::scanf("%u", &count) == 1);
        tpde::AssignmentAllocInfo info(count);
        std::printf("I %u %u %u %u\n", count, info.size, info.alloc_size, info.free_list_idx);
        break;
      }
      case 'A': {
        unsigned slot, count, mode, seed, flags;
        require(std::scanf("%u %u %u %u %u", &slot, &count, &mode, &seed, &flags) == 5);
        require(slot < slots.size() && !slots[slot]);
        auto *value = mode == 0 ? allocator.allocate(count)
                               : allocator.allocate_slow(count, mode == 2);
        if (mode == 2) zero_snapshot(value);
        value->frame_off = std::bit_cast<int32_t>(seed ^ uint32_t{0x87654321});
        value->part_count = count;
        value->references_left = std::rotl(seed, 11);
        value->max_part_size = uint8_t(seed);
        value->pending_free = flags & 1;
        value->variable_ref = flags & 2;
        value->stack_variable = flags & 4;
        value->delay_free = flags & 8;
        for (uint32_t i = 0; i < count; ++i) value->parts[i] = part(seed, i);
        slots[slot] = value;
        snapshot(op, slot, value);
        break;
      }
      case 'M': {
        unsigned slot, field, value;
        require(std::scanf("%u %u %u", &slot, &field, &value) == 3);
        require(slot < slots.size() && slots[slot]);
        auto *assignment = slots[slot];
        switch (field) {
        case 0: assignment->frame_off = std::bit_cast<int32_t>(value); break;
        case 1: assignment->var_ref_custom_idx = value; break;
        case 2: assignment->references_left = value; break;
        case 3: assignment->next_delayed_free_entry = tpde::ValLocalIdx(value); break;
        case 4: assignment->max_part_size = uint8_t(value); break;
        case 5: assignment->pending_free = value != 0; break;
        case 6: assignment->variable_ref = value != 0; break;
        case 7: assignment->stack_variable = value != 0; break;
        case 8: assignment->delay_free = value != 0; break;
        default: require(field - 9 < assignment->part_count); assignment->parts[field - 9] = uint16_t(value); break;
        }
        snapshot(op, slot, assignment);
        break;
      }
      case 'F': {
        unsigned slot, slow;
        require(std::scanf("%u %u", &slot, &slow) == 2);
        require(slot < slots.size() && slots[slot]);
        retired[slot] = reinterpret_cast<const unsigned char *>(slots[slot]);
        if (slow) allocator.deallocate_slow(slots[slot]); else allocator.deallocate(slots[slot]);
        slots[slot] = nullptr;
        std::printf("F %u %u %zu %zu\n", slot, slow, live_slabs, live_bytes);
        break;
      }
      case 'R':
        for (auto *value : slots) require(!value);
        allocator.reset();
        std::printf("R %zu %zu\n", live_slabs, live_bytes);
        break;
      case 'P': {
        unsigned slot, offset, use_retired;
        require(std::scanf("%u %u %u", &slot, &offset, &use_retired) == 3);
        require(slot < slots.size());
#ifdef TPDE_ASAN_BUILD
        const auto *pointer = use_retired ? retired[slot] : reinterpret_cast<const unsigned char *>(slots[slot]);
        require(pointer);
        std::printf("P %u %u %u %u\n", slot, offset, use_retired,
            unsigned(__asan_address_is_poisoned(pointer + offset) != 0));
#else
        std::abort();
#endif
        break;
      }
      default: std::abort();
      }
    }
    require(std::feof(stdin));
  }
  require(live_slabs == 0 && live_bytes == 0);
  std::puts("E 0 0");
}
